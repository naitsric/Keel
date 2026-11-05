//! Hides known secrets in text about to be shown or sent out. It only ever works on a copy: the
//! audit record and what agents receive keep the original.
//!
//! It goes by shapes and names, so some secrets slip through. It never guesses from how random a
//! string looks, because that would eat commit hashes.

use std::sync::LazyLock;

use regex::{Captures, Regex};
use serde::{Serialize, Serializer};
use serde_json::Value;

/// What stands in for a secret. It has no words, so it needs no translating.
pub const MASK: &str = "••••••";

fn compiled(pattern: &str) -> Regex {
    Regex::new(pattern).expect("a valid pattern")
}

/// A key cut off by a limit has no end line, so its body also ends where base64 does. That way a
/// mention of the header in code doesn't take the rest of the text with it.
static PEM_BLOCK: LazyLock<Regex> = LazyLock::new(|| {
    compiled(
        r"-----BEGIN [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----(?:(?s:.*?)-----END [A-Z0-9 ]*PRIVATE KEY(?: BLOCK)?-----|(?:[A-Za-z0-9+/=\s-]|\\[nr])*)",
    )
});

static TOKEN: LazyLock<Regex> = LazyLock::new(|| {
    compiled(concat!(
        r"\b(?:gh[opsu]_[A-Za-z0-9]{16,}",
        r"|github_pat_[A-Za-z0-9_]{16,}",
        r"|sk-ant-[A-Za-z0-9_-]{16,}",
        r"|xox[abprs]-[A-Za-z0-9-]{10,}",
        r"|AKIA[0-9A-Z]{16,}",
        r"|glpat-[A-Za-z0-9_.-]{16,}",
        r"|npm_[A-Za-z0-9]{32,}",
        r"|AIza[A-Za-z0-9_-]{30,}",
        r"|eyJ[A-Za-z0-9_-]{4,}\.eyJ[A-Za-z0-9_-]{4,}\.[A-Za-z0-9_-]*)",
    ))
});

/// Prefixes people also start ordinary names with, so these get a second look.
static TOKEN_OR_NAME: LazyLock<Regex> = LazyLock::new(|| {
    compiled(r"\b(?:sk-[A-Za-z0-9_-]{20,}|keel_[A-Za-z0-9_-]{32,}|idedb_[A-Za-z0-9_-]{16,})")
});

static URL_PASSWORD: LazyLock<Regex> =
    LazyLock::new(|| compiled(r#"(?i)\b([a-z][a-z0-9+.-]*://[^\s:/?#@'"`\\]*):([^\s/?#'"`\\]+)@"#));

static AUTHORIZATION: LazyLock<Regex> = LazyLock::new(|| {
    compiled(
        r#"(?i)\b((?:authorization\\?["'`]?[ \t]*[:=][ \t]*\\?["'`]?[ \t]*(?:bearer|basic|token)|bearer)[ \t]+)([^\s"'`,;\\]+)"#,
    )
});

const SECRET_NAME: &str = concat!(
    r"(?:[A-Za-z0-9_.-]*[a-z0-9](?:Key|Secret|Token|Pwd)",
    r"|(?i:[a-z0-9_.-]*[_-](?:key|secret|token|pwd|pass)|[a-z0-9_.-]*(?:password|passwd|passphrase))",
    r"|\b(?i:api_?key|secret|token))",
);

/// A name, the end of its own quotes, what assigns it (past a type like `: str`), and its value.
static ASSIGNMENT: LazyLock<Regex> = LazyLock::new(|| {
    compiled(&format!(
        concat!(
            r#"({name})(\\?["'`]\]?[ \t]*|\])?"#,
            r"((?::[ \t]*[&'A-Za-z0-9_<>\[\]|., ]{{1,40}}?)?[ \t]*(?::=|=>|=)[ \t]*|:[ \t]*)",
            r#"([bBrRuU]{{0,2}}"(?:[^"\\\n]|\\.)*"|[bBrRuU]{{0,2}}'[^'\n]*'|`[^`\n]*`"#,
            r#"|\\"(?:[^"\\\n]|\\[^"\n])*\\"|[^\s"'`=:\\](?:[^\s"'`\\]|\\[^nrt"'\s])*)"#,
        ),
        name = SECRET_NAME,
    ))
});

static WHOLE_SECRET_NAME: LazyLock<Regex> = LazyLock::new(|| compiled(&format!("^{SECRET_NAME}$")));

/// A variable or a placeholder standing where the value would be.
static REFERENCE: LazyLock<Regex> = LazyLock::new(|| {
    compiled(r"^(?:\$[A-Za-z_{(]|\$\d+$|\{[\w.:-]*\}$|<[^<>]*>$|%\(?\w*\)?[sd]$|\?$)")
});

/// A number literal or a number's type. Long hex stays out: it's the shape of some private keys.
static NUMBER: LazyLock<Regex> = LazyLock::new(|| {
    compiled(
        r"^(?:0[xX][0-9a-fA-F_]{1,16}|[0-9_]+[iuf](?:8|16|32|64|128|size)|[iuf](?:8|16|32|64|128|size))$",
    )
});

static CALL_OR_INDEX: LazyLock<Regex> = LazyLock::new(|| compiled(r"^[A-Za-z0-9_.:]*[(\[!]"));

static PATH_OF_NAMES: LazyLock<Regex> = LazyLock::new(|| {
    compiled(r"^[A-Za-z_][A-Za-z0-9_]*(?:(?:[?!]?\.|::|->)[A-Za-z_][A-Za-z0-9_]*)+$")
});

/// A language's own words and types, and what a schema or a form says instead of a value.
const NOT_A_VALUE: [&str; 20] = [
    "none",
    "null",
    "nil",
    "undefined",
    "true",
    "false",
    "str",
    "string",
    "int",
    "integer",
    "bool",
    "boolean",
    "bytes",
    "number",
    "float",
    "any",
    "object",
    "void",
    "required",
    "optional",
];

/// `*_KEY` names that are ordinarily not secret.
const ORDINARY_KEYS: [&str; 14] = [
    "sort",
    "primary",
    "foreign",
    "partition",
    "range",
    "hash",
    "unique",
    "index",
    "cache",
    "storage",
    "public",
    "query",
    "i18n",
    "group",
];

/// Letters and separators, without a digit: what a label looks like, and what no generated
/// credential does.
fn is_label(value: &str) -> bool {
    value
        .bytes()
        .all(|b| b.is_ascii_alphabetic() || b"-_.:/@ ".contains(&b))
}

/// `sk-learn-some-long-name` and `keel_engine_some_long_name` are names, not tokens.
fn reads_as_a_name(token: &str) -> bool {
    let body = token.split_once(['-', '_']).map_or(token, |(_, rest)| rest);
    body.contains(['-', '_']) && is_label(body)
}

fn is_reference(value: &str) -> bool {
    value.contains("${") || value.contains("{{") || REFERENCE.is_match(value)
}

fn is_path(value: &str) -> bool {
    ["/", "./", "../", "~/"]
        .iter()
        .any(|start| value.starts_with(start))
}

/// Whether a short label assigned to this name can be just a label. `token` is an everyday word in
/// code and `*_KEY` names plenty that isn't secret, so a constant like `STORAGE_KEY = "theme"` or a
/// field like `sort_key: name` stays. Anywhere else, the name says it's a secret.
fn can_hold_a_label(lower_name: &str, operator: &str, is_upper: bool) -> bool {
    if lower_name == "token" {
        return true;
    }
    if !lower_name.ends_with("key")
        || ["api", "secret", "private", "access", "auth"]
            .iter()
            .any(|word| lower_name.contains(word))
    {
        return false;
    }
    let is_code = operator.contains('=') && operator != "=";
    is_code || (!is_upper && ORDINARY_KEYS.iter().any(|word| lower_name.contains(word)))
}

/// Whether what's assigned to a secret's name is the secret, rather than code that gets it.
/// `operator` is what assigns it, and `rest` is the rest of its line.
fn is_secret_value(name: &str, value: &str, operator: &str, quoted: bool, rest: &str) -> bool {
    if value.is_empty() || value.contains(MASK) || is_reference(value) || is_path(value) {
        return false;
    }
    let lower_name = name.to_ascii_lowercase();
    let is_password = ["pass", "pwd"].iter().any(|w| lower_name.contains(w));
    // A count or a place in a file: `MAX_TOKEN = 4096`, `src/token:42:7`.
    if !is_password && value.len() <= 6 && value.bytes().all(|b| b.is_ascii_digit() || b == b':') {
        return false;
    }
    if NOT_A_VALUE.contains(&value.to_ascii_lowercase().as_str()) || NUMBER.is_match(value) {
        return false;
    }
    let is_upper = !name.bytes().any(|b| b.is_ascii_lowercase());
    if is_label(value) && value.len() < 32 && can_hold_a_label(&lower_name, operator, is_upper) {
        return false;
    }
    if quoted {
        return true;
    }
    if CALL_OR_INDEX.is_match(value) || name.eq_ignore_ascii_case(value) {
        return false;
    }
    // `NAME=word` can't be a variable in a shell or a .env file: it's the word itself.
    if is_upper && operator == "=" {
        return true;
    }
    if value.starts_with(['&', '*', '{', '!', '|']) || value.contains(['<', '>']) {
        return false;
    }
    if PATH_OF_NAMES.is_match(value) {
        return false;
    }
    if !value.bytes().all(|b| b.is_ascii_alphabetic() || b == b'_') {
        return true;
    }
    // Only letters: an identifier, unless it's a lone lowercase word. Then it's `password = entered`
    // in code or `password: entered` in YAML, and a sentence has more words after it.
    if !value.bytes().all(|b| b.is_ascii_lowercase()) {
        return is_upper && !operator.contains('=');
    }
    if operator.contains('=') {
        return operator == "=";
    }
    let rest = rest.trim_start();
    rest.is_empty() || rest.starts_with('#')
}

/// A value's opening quote, what's inside and what closes it. Unquoted, the punctuation that ends
/// it in code stays outside, which leaves the opening part empty.
fn split_value(value: &str) -> (&str, &str, &str) {
    let body = value.trim_start_matches(['b', 'B', 'r', 'R', 'u', 'U']);
    for quote in ["\\\"", "\"", "'", "`"] {
        if let Some(inner) = body
            .strip_prefix(quote)
            .and_then(|rest| rest.strip_suffix(quote))
        {
            let opening = value.len() - inner.len() - quote.len();
            return (&value[..opening], inner, quote);
        }
    }
    let inner = value.trim_end_matches([',', ';', ':', ')', ']', '}']);
    ("", inner, &value[inner.len()..])
}

fn mask_assignment(caps: &Captures, text: &str) -> String {
    let whole = caps.get(0).expect("the whole match");
    let value = &caps[4];
    let (opening, inner, closing) = split_value(value);
    let rest = text[whole.end()..].lines().next().unwrap_or("");
    if !is_secret_value(&caps[1], inner, &caps[3], !opening.is_empty(), rest) {
        return whole.as_str().to_owned();
    }
    let head = &whole.as_str()[..whole.len() - value.len()];
    format!("{head}{opening}{MASK}{closing}")
}

/// Whether a diff adds something that looks like a secret. Masking hides it on screen, but approving
/// would still push it with the branch. Lines being removed don't count: they leave the code.
pub fn diff_adds_secret(diff: &str) -> bool {
    let added = diff
        .lines()
        .filter(|line| !line.starts_with("+++"))
        .filter_map(|line| line.strip_prefix('+'))
        .collect::<Vec<_>>()
        .join("\n");
    mask_secrets(&added) != added
}

/// `text` with the secrets it can recognize replaced by [`MASK`].
pub fn mask_secrets(text: &str) -> String {
    let text = PEM_BLOCK.replace_all(text, MASK);
    let text = TOKEN.replace_all(&text, MASK);
    let text = TOKEN_OR_NAME.replace_all(&text, |caps: &Captures| {
        if reads_as_a_name(&caps[0]) {
            caps[0].to_owned()
        } else {
            MASK.to_owned()
        }
    });
    let text = URL_PASSWORD.replace_all(&text, |caps: &Captures| {
        if is_reference(&caps[2]) || caps[1].contains("{{") {
            caps[0].to_owned()
        } else {
            format!("{}:{MASK}@", &caps[1])
        }
    });
    let text = AUTHORIZATION.replace_all(&text, |caps: &Captures| {
        let credential = &caps[2];
        let bare = caps[1].to_ascii_lowercase().starts_with("bearer");
        let too_plain = bare && (credential.len() < 16 || is_label(credential));
        if too_plain || is_reference(credential) {
            caps[0].to_owned()
        } else {
            format!("{}{MASK}", &caps[1])
        }
    });
    let masked = ASSIGNMENT.replace_all(&text, |caps: &Captures| mask_assignment(caps, &text));
    masked.into_owned()
}

/// In JSON a name and its value are apart, where the text patterns can't see them together.
fn hides_whole_value(name: &str, text: &str) -> bool {
    if name.eq_ignore_ascii_case("authorization") {
        let credential = text.rsplit(' ').next().unwrap_or(text);
        return is_secret_value(name, credential, ": ", true, "");
    }
    WHOLE_SECRET_NAME.is_match(name) && is_secret_value(name, text, ": ", true, "")
}

/// `value` with every string masked, at any depth, keys included.
pub fn mask_value(value: Value) -> Value {
    match value {
        Value::String(text) => Value::String(mask_secrets(&text)),
        Value::Array(items) => Value::Array(items.into_iter().map(mask_value).collect()),
        Value::Object(fields) => {
            // A project's variables are recorded as `{ "name": ..., "value": ... }`.
            let pair_name = fields
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_owned);
            Value::Object(
                fields
                    .into_iter()
                    .map(|(key, value)| {
                        let name = match &pair_name {
                            Some(name) if key == "value" => name.as_str(),
                            _ => key.as_str(),
                        };
                        let hidden =
                            matches!(&value, Value::String(text) if hides_whole_value(name, text));
                        let value = if hidden {
                            Value::String(MASK.to_owned())
                        } else {
                            mask_value(value)
                        };
                        (mask_secrets(&key), value)
                    })
                    .collect(),
            )
        }
        other => other,
    }
}

/// Serializes `value` with its secrets masked, for `#[serde(serialize_with = "...")]`.
pub fn serialize_masked<T: Serialize, S: Serializer>(
    value: &T,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    let json = serde_json::to_value(value).map_err(serde::ser::Error::custom)?;
    mask_value(json).serialize(serializer)
}

/// A value that hides its secrets when it's serialized, so nothing receiving it can forget to.
#[derive(Debug)]
pub struct Masked<T>(pub T);

impl<T: Serialize> Serialize for Masked<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serialize_masked(&self.0, serializer)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn a_diff_counts_as_adding_a_secret_only_for_the_lines_it_adds() {
        let value = ["made", "up", "value", "123456"].join("-");
        let added = format!("--- a/.env\n+++ b/.env\n@@ -0,0 +1 @@\n+API_KEY={value}\n");
        assert!(diff_adds_secret(&added));

        let removed = format!("--- a/.env\n+++ b/.env\n@@ -1 +0,0 @@\n-API_KEY={value}\n");
        assert!(!diff_adds_secret(&removed));

        let context = format!("@@ -1,2 +1,2 @@\n API_KEY={value}\n-a = 1\n+a = 2\n");
        assert!(!diff_adds_secret(&context));

        let plain =
            "--- a/calc.py\n+++ b/calc.py\n@@ -1 +1,2 @@\n+def triple(x):\n+    return x * 3\n";
        assert!(!diff_adds_secret(plain));
        assert!(!diff_adds_secret(""));
    }

    #[test]
    fn a_private_key_added_across_lines_counts() {
        let header = ["-----BEGIN ", "PRIVATE KEY-----"].concat();
        let footer = ["-----END ", "PRIVATE KEY-----"].concat();
        let diff = format!(
            "+++ b/key.pem\n@@ -0,0 +1,3 @@\n+{header}\n+QUJDREVGR0hJSktMTU5PUA==\n+{footer}\n"
        );
        assert!(diff_adds_secret(&diff));
    }

    /// Made up, and put together here so that no secret scanner trips on this file.
    fn fake(prefix: &str, piece: &str, times: usize) -> String {
        format!("{prefix}{}", piece.repeat(times))
    }

    const VALUE: &str = "made-up-Value-123";

    fn assert_hidden(templates: &[&str], value: &str) {
        for template in templates {
            let input = template.replace("{v}", value);
            assert_eq!(
                mask_secrets(&input),
                template.replace("{v}", MASK),
                "{template}"
            );
        }
    }

    #[test]
    fn a_dotenv_file_an_agent_read_keeps_its_names_and_loses_its_secrets() {
        let file = |secret: &str, url_password: &str| {
            format!(
                "     1\t# Local settings\n     2\tAPI_KEY={secret}\n     3\tDB_PASSWORD=\"{secret}\"\n     4\tDATABASE_URL=postgres://app:{url_password}@db:5432/orders\n     5\tPORT=3000\n     6\tDEBUG=true\n"
            )
        };
        assert_eq!(mask_secrets(&file(VALUE, VALUE)), file(MASK, MASK));
    }

    #[test]
    fn known_token_shapes_are_hidden() {
        let tokens = [
            fake("gho_", "x", 36),
            fake("ghp_", "x", 36),
            fake("ghs_", "x", 36),
            fake("ghu_", "x", 36),
            fake("github_pat_", "Fake0_", 12),
            fake("sk-ant-api03-", "Fake0-", 12),
            fake("sk-", "Fake0", 10),
            fake("sk-proj-", "Fake0_", 10),
            fake("sk-", "x", 40),
            fake("xoxb-", "1234-fake-", 3),
            fake("xoxa-", "1234-fake-", 3),
            fake("xoxp-", "1234-fake-", 3),
            fake("xoxr-", "1234-fake-", 3),
            fake("xoxs-", "1234-fake-", 3),
            fake("AKIA", "FAKE", 4),
            fake("glpat-", "Fake0", 5),
            fake("npm_", "Fake0", 8),
            fake("keel_", "Fake0-_", 7),
            fake("keel_", "x", 43),
            fake("idedb_", "Fake0", 6),
            fake("AIza", "Fake0_", 7),
            format!(
                "{}.{}.{}",
                fake("eyJ", "fake", 5),
                fake("eyJ", "fake", 8),
                "signature".repeat(4)
            ),
        ];
        for token in &tokens {
            for wrapped in [
                token.clone(),
                format!("the token is {token}, as printed"),
                format!("TOKEN_NAME_ONLY ({token})"),
                format!("\"{token}\""),
                format!("curl https://{token}@github.com/acme/app.git"),
            ] {
                let masked = mask_secrets(&wrapped);
                assert!(!masked.contains(token), "{masked}");
                assert!(masked.contains(MASK), "{masked}");
                assert_eq!(masked, wrapped.replace(token, MASK));
            }
        }
    }

    #[test]
    fn private_key_blocks_are_hidden_with_or_without_their_end() {
        let begin = |kind: &str| format!("-----BEGIN {kind} KEY-----");
        let end = |kind: &str| format!("-----END {kind} KEY-----");
        let body = "ZmFrZS1ieXRlcw==\nbm90LXJlYWw=";
        for kind in [
            "PRIVATE",
            "RSA PRIVATE",
            "EC PRIVATE",
            "OPENSSH PRIVATE",
            "ENCRYPTED PRIVATE",
        ] {
            let block = format!("{}\n{body}\n{}", begin(kind), end(kind));
            assert_eq!(
                mask_secrets(&format!("before\n{block}\nafter")),
                format!("before\n{MASK}\nafter")
            );
        }
        // In a diff, and written into JSON or a .env with `\n`.
        let added = format!(
            "+{}\n+ZmFrZS1ieXRlcw==\n+{}\n unchanged",
            begin("RSA PRIVATE"),
            end("RSA PRIVATE")
        );
        assert_eq!(mask_secrets(&added), format!("+{MASK}\n unchanged"));
        let escaped = format!(
            "\"key\": \"{}\\nZmFrZQ==\\n{}\\n\", \"other\": 1",
            begin("PRIVATE"),
            end("PRIVATE")
        );
        assert_eq!(
            mask_secrets(&escaped),
            format!("\"key\": \"{MASK}\\n\", \"other\": 1")
        );
        // Cut off by a limit before its end, it still doesn't show.
        let cut = format!("{}\nZmFrZS1ieXRlcw==\nbm90", begin("PRIVATE"));
        assert_eq!(mask_secrets(&cut), MASK);
        // A mention of the header in code leaves what comes after it alone.
        let mention = format!(
            "if key.startswith(\"{}\"):\n    load(key)",
            begin("PRIVATE")
        );
        assert_eq!(
            mask_secrets(&mention),
            format!("if key.startswith(\"{MASK}\"):\n    load(key)")
        );
    }

    #[test]
    fn a_password_in_a_url_goes_and_the_user_and_host_stay() {
        assert_hidden(
            &[
                "postgres://app:{v}@db.internal:5432/orders",
                "mysql://root:{v}@127.0.0.1/db",
                "redis://:{v}@cache:6379/0",
                "https://x-access-token:{v}@github.com/acme/app.git",
                "fatal: unable to access 'https://bot:{v}@example.com/a.git/': denied",
                "DATABASE_URL=postgres://app:{v}@db/orders",
                "\"url\": \"amqp://guest:{v}@rabbit\"",
            ],
            VALUE,
        );
        // Not encoded, an @ in the password doesn't leave its end showing.
        assert_eq!(
            mask_secrets(&format!("postgres://app:ab@{VALUE}@db/x")),
            format!("postgres://app:{MASK}@db/x")
        );
    }

    #[test]
    fn authorization_headers_are_hidden() {
        assert_hidden(
            &[
                "Authorization: Bearer {v}",
                "authorization: bearer {v}",
                "Authorization: Basic {v}",
                "Authorization: token {v}",
                "Proxy-Authorization: Basic {v}",
                "curl -H \"Authorization: Bearer {v}\" https://example.com",
                "curl -H 'Authorization: Basic {v}' https://example.com",
                "{\"Authorization\":\"Bearer {v}\"}",
                "{\\\"Authorization\\\":\\\"Bearer {v}\\\"}",
                "headers = { Authorization = \"Bearer {v}\" }",
                "Bearer {v}",
            ],
            VALUE,
        );
    }

    #[test]
    fn values_assigned_to_secret_names_are_hidden_in_every_form() {
        let templates = [
            // .env and shell, quoted or not.
            "API_KEY={v}",
            "OPENAI_API_KEY={v}",
            "export GITHUB_TOKEN={v}",
            "DB_PASSWORD={v}",
            "PASSWORD={v}",
            "DB_PASSWD={v}",
            "DB_PWD={v}",
            "DB_PASS={v}",
            "PASSPHRASE={v}",
            "CLIENT_SECRET={v}",
            "AWS_SECRET_ACCESS_KEY={v}",
            "STRIPE_KEY={v}",
            "MY_TOKEN=\"{v}\"",
            "MY_TOKEN='{v}'",
            "API_KEY={v} curl https://api.example.com",
            "$env:API_KEY = \"{v}\"",
            // YAML.
            "api_key: {v}",
            "password: {v}",
            "password: {v} # the real one",
            "db_password: \"{v}\"",
            "  client_secret: '{v}'",
            "token: {v}",
            // JSON, and JSON inside JSON.
            "{\"api_key\":\"{v}\"}",
            "\"password\" : \"{v}\",",
            "{\\\"token\\\":\\\"{v}\\\"}",
            "{\"accessToken\":\"{v}\",\"other\":1}",
            // Code.
            "const API_KEY = \"{v}\";",
            "API_KEY = '{v}'",
            "api_key: str = \"{v}\"",
            "const SECRET: &str = \"{v}\";",
            "os.environ[\"API_KEY\"] = \"{v}\"",
            "app.secret_key = b'{v}'",
            "password => '{v}'",
            "password := \"{v}\"",
            "connect(password={v}, host=db)",
            "apiKey: `{v}`",
            // A diff, a flag and a header.
            "+API_KEY={v}",
            "-password: {v}",
            "--api-key={v}",
            "X-Api-Key: {v}",
            "curl -H 'X-Api-Key: {v}' https://example.com",
            // Written into JSON, where a line break is two characters.
            "API_KEY={v}\\nand the rest stays",
        ];
        assert_hidden(&templates, VALUE);
    }

    #[test]
    fn weak_values_are_still_values_where_a_word_cannot_be_code() {
        assert_hidden(
            &[
                "PASSWORD={v}",
                "MY_KEY={v}",
                "MY_TOKEN={v}",
                "API_KEY={v}",
                "password: {v}",
                "password: {v} # the old one",
                "api_key: {v}",
                "\"password\": \"{v}\"",
                "API_KEY = \"{v}\"",
                "password={v}",
                "my_key: {v}",
                "stripe_key: {v}",
                "\"my_key\": \"{v}\"",
                "MY_KEY: {v}",
                "\"MY_KEY\": \"{v}\"",
                "my_secret: {v}",
                "my_token: {v}",
                "secret: {v}",
                "\"secret\": \"{v}\"",
            ],
            "plainword",
        );
        assert_eq!(
            mask_secrets("DB_PASSWORD=1234"),
            format!("DB_PASSWORD={MASK}")
        );
        assert_eq!(mask_secrets("password: 1234"), format!("password: {MASK}"));
        let hex = fake("0x", "abcd0123", 8);
        assert_eq!(
            mask_secrets(&format!("PRIVATE_KEY={hex}")),
            format!("PRIVATE_KEY={MASK}")
        );
    }

    #[test]
    fn what_is_not_a_secret_is_left_alone() {
        let survivors = [
            // Hashes and ids.
            "28c696fd11223344556677889900aabbccddeeff",
            "commit 28c696fd11223344556677889900aabbccddeeff",
            "9f2c4e1ab7d04c55e6a1f0b3c28d7e6fa1b2c3d4e5f60718293a4b5c6d7e8f90",
            "sha256: 9f2c4e1ab7d04c55e6a1f0b3c28d7e6fa1b2c3d4e5f60718293a4b5c6d7e8f90",
            "index 12ee743..4693ad3 100644",
            "@@ -1,2 +1,2 @@",
            "5e882d0f-0000-4000-8000-000000000000",
            "session_id: 5e882d0f-0000-4000-8000-000000000000",
            // Code that gets a secret from somewhere.
            "token = get_token()",
            "let token = gh.token(&account).map_err(delivery_failed)?;",
            "let token = format!(\"{PREFIX}{}\", encode(bytes));",
            "password: str",
            "password: String,",
            "password: Option<String>",
            "password?: string",
            "api_key: &str",
            "token: &token,",
            "token: string | null",
            "token: Token = Depends(get_token)",
            "api_key: str = None",
            "def f(password: str = None):",
            "api_key=None",
            "api_key=api_key",
            "password=password",
            "password = user_input",
            "password = entered",
            "api_key = self.api_key",
            "api_key = config.api_key,",
            "api_key: ApiKey",
            "password: SecretStr",
            "secret = os.environ[\"SECRET\"]",
            "secret = os.environ.get(\"SECRET\")",
            "API_KEY=os.getenv(\"API_KEY\")",
            "const token = await getToken();",
            "password: await hash(entered)",
            "if password == other:",
            "if token != other:",
            "token => token.id",
            "Token::new()",
            "token[1:]",
            "x ? token : other",
            // Names that only look like it.
            "const TOKEN_LIMIT = 5",
            "TOKEN_LIMIT=5",
            "MAX_TOKEN = 4096",
            "token_count = 5",
            "inputTokens: 5",
            "apiKeySource: none",
            "tokenPrefix: abc",
            "password_hash = compute(entered)",
            "PRIMARY_KEY = 1",
            "pub const VK_F22: VIRTUAL_KEY = 133u16;",
            "pub const IW_AUTH_ALG_SHARED_KEY: c_ulong = 0x2;",
            "fn remove(&self, token: i64) -> Result<()>;",
            "primary_key=True",
            "sort_key: name",
            "const LANGUAGE_KEY: &str = \"language\";",
            "const STORAGE_KEY = \"keel-language\";",
            "const BACKGROUND_VIEW_KEY: &str = \"BackgroundView\";",
            "this.altKey = init?.altKey ?? false;",
            "cache_key = \"user:profile\"",
            "\"token\": \"keyword\"",
            "\"password\": \"string\"",
            "state === \"blocked\" ? \"ANTHROPIC_API_KEY\" : \"none\"",
            // References and placeholders.
            "$API_KEY",
            "${SECRET}",
            "{{socketDir:db}}",
            "process.env.API_KEY",
            "API_KEY=$API_KEY",
            "API_KEY=${API_KEY}",
            "API_KEY=\"$API_KEY\"",
            "TOKEN=$(gh auth token)",
            "password = $1",
            "password = ?",
            "password = %s",
            "\"password\": \"{password}\"",
            "API_KEY: process.env.API_KEY,",
            "const key = process.env.API_KEY;",
            "GITHUB_TOKEN: ${{ secrets.GITHUB_TOKEN }}",
            "api_key: \"{{ vault_api_key }}\"",
            "API_KEY=<your-key-here>",
            "DATABASE_URL=postgresql://postgres@/test?host={{socketDir:db}}",
            "Authorization: Bearer $TOKEN",
            "Authorization: Bearer ${token}",
            "Authorization: Bearer <token>",
            "headers = { Authorization: `Bearer ${token}` }",
            "password: \"\"",
            "PASSWORD=",
            // Words.
            "password: the one you chose",
            "Password: required",
            "Bearer tokens are issued by the server.",
            "bearer tokens-issued-by-the-server",
            "Use a token: it expires.",
            "the disk-usage-report-for-every-machine",
            "sk-learn-is-a-long-package-name",
            "npm_package_name",
            "npm_config_registry=https://registry.npmjs.org",
            "keel_core::secrets::mask_value",
            "keel_engine_some_really_long_internal_name",
            "keel_Ab3xYz",
            "idedb_connection_manager_instance",
            // Keys that aren't private.
            "-----BEGIN PUBLIC KEY-----",
            "-----BEGIN CERTIFICATE-----\nZmFrZQ==\n-----END CERTIFICATE-----",
            // Paths.
            "/Users/you/Library/Application Support/io.github.naitsric.keel/runs/r1/builder-1.jsonl",
            "src/keys/api_key.rs:42:7: error",
            "src/token:42:7: error",
            "tests/test_secret_key.py",
            "config/secrets.yml",
            "~/.ssh/id_rsa",
            "SSH_KEY=/Users/you/.ssh/id_rsa",
            "TLS_KEY: ./certs/key.pem",
            // URLs without a password.
            "https://github.com/acme/app/pull/12",
            "http://localhost:3000/path?x=a@b",
            "http://127.0.0.1:7413/mcp",
            "https://user@example.com/x",
            "https://example.com:8443/#fragment",
            "git@github.com:acme/app.git",
            "ssh://git@github.com:22/acme/app.git",
            "http://[::1]:8080/",
            "postgres://user:${DB_PASSWORD}@host/db",
            "postgres://user:{{password}}@host/db",
            "postgres://user:$DB_PASSWORD@host/db",
            "postgresql://{{socketDir:db}}@/test",
            // A person's account, as an attempt records it.
            "you@example.com (max)",
        ];
        let changed: Vec<(&str, String)> = survivors
            .iter()
            .map(|text| (*text, mask_secrets(text)))
            .filter(|(text, masked)| text != masked)
            .collect();
        assert!(changed.is_empty(), "{changed:#?}");
    }

    #[test]
    fn masking_twice_changes_nothing_more() {
        let text = format!(
            "API_KEY={VALUE} postgres://a:{VALUE}@b Authorization: Bearer {VALUE} {} {{\"password\":\"{VALUE}\"}}",
            fake("ghp_", "x", 36)
        );
        let once = mask_secrets(&text);
        assert!(!once.contains(VALUE), "{once}");
        assert_eq!(mask_secrets(&once), once);
    }

    #[test]
    fn every_string_in_a_value_is_masked_at_any_depth_and_nothing_else_changes() {
        let token = fake("ghp_", "x", 36);
        let masked = mask_value(json!({
            "text": format!("printed {token}"),
            "seq": 7,
            "cost": 0.25,
            "ok": true,
            "missing": null,
            "nested": { "list": [format!("API_KEY={VALUE}"), 3, { "deep": token.clone() }] },
            "sha": "28c696fd11223344556677889900aabbccddeeff",
            token.clone(): "a token used as a key",
        }));
        assert_eq!(
            masked,
            json!({
                "text": format!("printed {MASK}"),
                "seq": 7,
                "cost": 0.25,
                "ok": true,
                "missing": null,
                "nested": { "list": [format!("API_KEY={MASK}"), 3, { "deep": MASK }] },
                "sha": "28c696fd11223344556677889900aabbccddeeff",
                MASK: "a token used as a key",
            })
        );
    }

    #[test]
    fn a_name_and_its_value_apart_in_json_are_seen_together() {
        let masked = mask_value(json!({
            "env": { "API_KEY": VALUE, "PATH": "/usr/bin", "DB_PASSWORD": "plainword" },
            "password": VALUE,
            "token": VALUE,
            "Authorization": format!("Bearer {VALUE}"),
            "services": [
                { "name": "DB_PASSWORD", "value": VALUE },
                { "name": "DATABASE_URL", "value": "postgresql://postgres@/test?host=/tmp" },
                { "name": "builder", "value": "still shown" },
            ],
        }));
        assert_eq!(
            masked,
            json!({
                "env": { "API_KEY": MASK, "PATH": "/usr/bin", "DB_PASSWORD": MASK },
                "password": MASK,
                "token": MASK,
                "Authorization": MASK,
                "services": [
                    { "name": "DB_PASSWORD", "value": MASK },
                    { "name": "DATABASE_URL", "value": "postgresql://postgres@/test?host=/tmp" },
                    { "name": "builder", "value": "still shown" },
                ],
            })
        );
    }

    #[test]
    fn a_reference_in_a_json_field_is_left_alone() {
        let value = json!({
            "api_key": "$API_KEY",
            "password": "${DB_PASSWORD}",
            "Authorization": "Bearer $TOKEN",
            "token": "keyword",
            "sortKey": "name",
            "apiKeySource": "ANTHROPIC_API_KEY",
            "inputTokens": 5,
            "tokenPrefix": "keel_Ab3xYz",
        });
        assert_eq!(mask_value(value.clone()), value);
    }

    #[test]
    fn a_masked_value_serializes_with_its_secrets_hidden() {
        #[derive(Serialize)]
        struct Shown {
            text: String,
            #[serde(serialize_with = "serialize_masked")]
            detail: Option<String>,
            plain: String,
        }
        let token = fake("ghp_", "x", 36);
        let shown = Shown {
            text: token.clone(),
            detail: Some(token.clone()),
            plain: "left".into(),
        };
        assert_eq!(
            serde_json::to_value(Masked(&shown)).unwrap(),
            json!({ "text": MASK, "detail": MASK, "plain": "left" })
        );
        assert_eq!(
            serde_json::to_value(&shown).unwrap(),
            json!({ "text": token, "detail": MASK, "plain": "left" })
        );
    }
}
