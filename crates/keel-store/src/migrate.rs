//! Bringing a database up to this build, and refusing one this build would damage.
//!
//! `PRAGMA user_version` only counts migrations. Two builds that disagree about what a number
//! means, as two branches that each added the next one do, would leave a database neither can
//! read, and nothing would notice. So every migration that runs leaves its SHA-256 beside it, and
//! a database is only opened when what ran on it is what this build has.

use std::path::Path;

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use sha2::{Digest, Sha256};

use crate::{Error, Result};

/// Outside the numbered list, so it takes no number and a database from before it gets it on its
/// next open.
const HASHES: &str = "create table if not exists migration_hash (
    number integer primary key,
    sha256 text not null
)";

fn hash(migration: &str) -> String {
    Sha256::digest(migration.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The hash each migration left when it ran, by number. Empty for a database from before they
/// were kept.
fn recorded(conn: &Connection) -> Result<Vec<(i64, String)>> {
    let kept: Option<i64> = conn
        .query_row(
            "select 1 from sqlite_master where type = 'table' and name = 'migration_hash'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if kept.is_none() {
        return Ok(Vec::new());
    }
    let mut statement =
        conn.prepare("select number, sha256 from migration_hash order by number")?;
    let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}

/// Whether this build can take the database on, and how many of its migrations the database has
/// had. One that a newer Keel opened, or where a migration that ran isn't this build's, is
/// refused. Only reads.
pub(crate) fn check(conn: &Connection, migrations: &[&str]) -> Result<usize> {
    let found: i64 = conn.query_row("pragma user_version", [], |row| row.get(0))?;
    let applied = usize::try_from(found).unwrap_or(usize::MAX);
    if applied > migrations.len() {
        return Err(Error::FromNewerKeel {
            found,
            known: migrations.len(),
        });
    }
    for (number, sha256) in recorded(conn)? {
        let ours = usize::try_from(number)
            .ok()
            .filter(|number| (1..=applied).contains(number))
            .map(|number| hash(migrations[number - 1]));
        // A hash beyond what ran says nothing about what's there now. It's written over when
        // that number runs.
        if ours.is_some_and(|ours| ours != sha256) {
            return Err(Error::FromAnotherKeel { number });
        }
    }
    Ok(applied)
}

/// Runs the migrations after the first `applied`, which `check` counted, and leaves the hash of
/// each one beside it.
pub(crate) fn apply(conn: &mut Connection, migrations: &[&str], applied: usize) -> Result<()> {
    let tx = conn.transaction()?;
    tx.execute_batch(HASHES)?;
    let record = "insert into migration_hash (number, sha256) values (?1, ?2)
        on conflict (number) do update set sha256 = excluded.sha256";
    // From before the hashes were kept, or by a build that didn't keep them: what's there is
    // taken as this build's, since nothing says otherwise.
    for (index, migration) in migrations.iter().enumerate().take(applied) {
        tx.execute(
            "insert or ignore into migration_hash (number, sha256) values (?1, ?2)",
            (index as i64 + 1, hash(migration)),
        )?;
    }
    for (index, migration) in migrations.iter().enumerate().skip(applied) {
        tx.execute_batch(migration)?;
        tx.execute(record, (index as i64 + 1, hash(migration)))?;
        tx.pragma_update(None, "user_version", index as i64 + 1)?;
    }
    tx.commit()?;
    Ok(())
}

/// A setting as the database at `path` has it, without opening it for use: nothing is migrated or
/// written. For saying why a database can't be opened in the person's language.
pub fn saved_setting(path: &Path, key: &str) -> Option<String> {
    let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()?;
    conn.query_row("select value from setting where key = ?1", [key], |row| {
        row.get(0)
    })
    .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MIGRATIONS, Store};

    /// The SHA-256 of each migration as it shipped, in order. A database that ran one is refused
    /// by a build where it's different, so these never change. A migration added at the end
    /// passes without being here. Add its hash once it's merged: the test below prints it.
    const SHIPPED: &[&str] = &[
        "c85870bcdd28d8b4c012fd9bc392bd69e3145af974c79cfa733218ede6fcd88e",
        "81540d1d041ac5949ba7438bd49c63103c160529996059b3cfb623c5ee0e4c0c",
        "1446b94d99912624a7000d3b0b6a3f8c15c57bd61a0b2097d61cbe36ca32f058",
        "e7d5b469c89e69766c89683987d74955c092d3d119b6a5f927bb58b4f3c8c1e1",
        "03cf193880e42e600339aef306491a3cfc2d2c93055e388200f4bfda88b05b7a",
        "ffd065a7e8fd0eb0ba0a4838b273a6dcf654cfb6a2f9f9fc8d40c0b9e0595973",
        "aebf794b60be40edc8224cdeac4142ff3ecb74c22b6a57a8d8de81c0956e6fff",
        "df274559473182b923ff6dc22506c52048beddbb004ba588d080390a19b5a434",
        "f61817333242ba01ec4607588cc623197037b7e550c1a8617ec511fbb68c496c",
        "853b508872507a8be45886fc14f314f6d3872d7e1cf5ce30bcfbb54f02f423f4",
        "710d46a022e55a9dac868f293acb35c4c449e29209fa605dc0a838f6fe58b264",
        "f97ebd848f9ab849ccf298eb5d6de2e8ed0e346c463aec302d6decbf0428f01d",
        "ee673a91ba634d3a8f218de1a7151e76bff0a21000c93e9f873a1bd9b6376e3b",
        "58198b0114e0dd0f890a0c77be0061eed88e83d5226af7689c521668b9b0f869",
        "f6a5f7a10d2efdb4c883050e0d968d17e26796a1f27838f30be396ce38126b68",
        "0566f6ee1e178c9c256de45f1ad3e243ddb0376e85d7f63235f211ccd09ae7d5",
        "cdb1ddec64eb5629ed02651205634f9b092093049cbdd142f913da9a09fffe2b",
        "710c5dfbb92e40a7828c6e2cead50d2c293c7c688965a1e638109745cfd7810d",
        "571a0b9eed8fd5d2bb33ae958b16f48199fe15ff536ba719b36840c748f45731",
        "374dce630ef3dc6f151cdd70f2b7d653e2ddc67f0534e36a1c1e528c484d3d8c",
        "84eb72f63738f6fb1698788b557dd2d55f0e2b7cec2c7ef09a135d0cd73c5355",
        "08a51892d6ba1df754800b4c73bc7e3b6771c60a4fad135d73ecd5bf8821ec4c",
        "eb544c5f688650d0811560a5a3a75bdf273e3c0da730473988f98cf7aaed86ca",
        "b45cc6943a7f47cba97e2f8b0bb6cf7ba7c19e983c85735cde20629edcab28c6",
    ];

    fn database() -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keel.db");
        (dir, path)
    }

    /// A database as a Keel from before the hashes left it, with `count` migrations run.
    fn left_by_an_older_keel(path: &Path, count: usize) {
        let conn = Connection::open(path).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").unwrap();
        for migration in &MIGRATIONS[..count] {
            conn.execute_batch(migration).unwrap();
        }
        conn.pragma_update(None, "user_version", count as i64)
            .unwrap();
        conn.execute(
            "insert into setting (key, value) values ('language', 'es')",
            [],
        )
        .unwrap();
    }

    fn version(path: &Path) -> i64 {
        Connection::open(path)
            .unwrap()
            .query_row("pragma user_version", [], |row| row.get(0))
            .unwrap()
    }

    /// A database nobody opened through `Store`, so it's as SQLite makes one: its own journal
    /// mode, not WAL, and hashes only if asked for. `count` is how many migrations it says it had.
    fn made_by_hand(path: &Path, migrations: &[&str], count: usize, with_hashes: bool) {
        let conn = Connection::open(path).unwrap();
        for migration in migrations {
            conn.execute_batch(migration).unwrap();
        }
        if with_hashes {
            conn.execute_batch(HASHES).unwrap();
            for (index, migration) in migrations.iter().enumerate() {
                conn.execute(
                    "insert into migration_hash (number, sha256) values (?1, ?2)",
                    (index as i64 + 1, hash(migration)),
                )
                .unwrap();
            }
        }
        conn.pragma_update(None, "user_version", count as i64)
            .unwrap();
    }

    /// Everything a refusal must leave as it was.
    #[derive(Debug, PartialEq)]
    struct Shape {
        version: i64,
        journal: String,
        schema: Vec<String>,
        hashes: Vec<(i64, String)>,
    }

    fn shape(path: &Path) -> Shape {
        let conn = Connection::open(path).unwrap();
        let schema = conn
            .prepare("select coalesce(sql, name) from sqlite_master order by name")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        Shape {
            version: version(path),
            journal: conn
                .query_row("pragma journal_mode", [], |row| row.get(0))
                .unwrap(),
            schema,
            hashes: recorded(&conn).unwrap(),
        }
    }

    fn keeps_hashes(shape: &Shape) -> bool {
        shape
            .schema
            .iter()
            .any(|sql| sql.contains("migration_hash"))
    }

    #[test]
    fn migrations_that_shipped_never_change() {
        assert!(
            MIGRATIONS.len() >= SHIPPED.len(),
            "A migration was taken out. Migrations are only ever appended: put it back."
        );
        for (index, shipped) in SHIPPED.iter().enumerate() {
            assert_eq!(
                hash(MIGRATIONS[index]),
                *shipped,
                "Migration {} isn't the one that shipped. Migrations are only ever appended: a \
                 database that ran this one would be refused by this build. Put it back as it \
                 was, in its place, and add a new one at the end.",
                index + 1
            );
        }
        for (index, migration) in MIGRATIONS.iter().enumerate().skip(SHIPPED.len()) {
            println!(
                "migration {} isn't in SHIPPED yet: {}",
                index + 1,
                hash(migration)
            );
        }
    }

    #[test]
    fn a_new_database_keeps_the_hash_of_every_migration_it_ran() {
        let (_dir, path) = database();
        Store::open(&path).unwrap();
        let conn = Connection::open(&path).unwrap();
        let hashes = recorded(&conn).unwrap();
        assert_eq!(hashes.len(), MIGRATIONS.len());
        for (number, sha256) in hashes {
            assert_eq!(sha256, hash(MIGRATIONS[number as usize - 1]));
        }
    }

    #[test]
    fn a_database_left_at_an_older_version_without_hashes_upgrades_and_gets_them() {
        let (_dir, path) = database();
        // Where the installed Keel's database was when the hashes arrived.
        left_by_an_older_keel(&path, 14);

        let store = Store::open(&path).unwrap();

        assert_eq!(store.setting("language").unwrap().as_deref(), Some("es"));
        drop(store);
        assert_eq!(version(&path), MIGRATIONS.len() as i64);
        let conn = Connection::open(&path).unwrap();
        let hashes = recorded(&conn).unwrap();
        assert_eq!(hashes.len(), MIGRATIONS.len());
        assert_eq!(hashes[0], (1, hash(MIGRATIONS[0])));
        assert_eq!(hashes[13], (14, hash(MIGRATIONS[13])));
        // And it opens again, now that there's something to compare.
        Store::open(&path).unwrap();
    }

    #[test]
    fn a_database_a_newer_keel_opened_is_refused_and_left_as_it_was() {
        let (_dir, path) = database();
        let mut newer = MIGRATIONS.to_vec();
        newer.push("create table from_a_newer_keel (id text primary key);");
        // Never opened by a Keel that keeps hashes, so a refusal that wrote anything would show:
        // there's no table for them, and the journal isn't the one Keel sets.
        made_by_hand(&path, &newer, newer.len(), false);
        let before = shape(&path);
        assert_eq!(before.journal, "delete");
        assert!(!keeps_hashes(&before));

        let Err(error) = Store::open(&path) else {
            panic!("a database from a newer Keel was opened")
        };

        assert!(matches!(
            error,
            Error::FromNewerKeel { found, known }
                if found == newer.len() as i64 && known == MIGRATIONS.len()
        ));
        assert!(error.to_string().contains("a newer Keel"), "{error}");
        // The version, the schema with no table for hashes, and how it journals.
        assert_eq!(shape(&path), before);
        // The Keel that left it there still opens it.
        Store::open_with(&path, &newer).unwrap();
        assert_eq!(shape(&path).journal, "wal");
    }

    #[test]
    fn a_database_where_a_migration_that_ran_differs_is_refused_and_left_as_it_was() {
        let (_dir, path) = database();
        // Two branches that each added the next migration: same number, different change.
        let mut ours = MIGRATIONS.to_vec();
        ours.push("create table from_this_branch (id text primary key);");
        let mut theirs = MIGRATIONS.to_vec();
        theirs.push("create table from_another_branch (id text primary key);");
        made_by_hand(&path, &theirs, theirs.len(), true);
        let before = shape(&path);
        assert_eq!(before.journal, "delete");
        assert_eq!(before.hashes.len(), theirs.len());

        let Err(error) = Store::open_with(&path, &ours) else {
            panic!("a database another branch changed was opened")
        };

        assert!(matches!(
            error,
            Error::FromAnotherKeel { number } if number == ours.len() as i64
        ));
        assert!(
            error.to_string().contains("a different version of Keel"),
            "{error}"
        );
        assert_eq!(shape(&path), before);
        Store::open_with(&path, &theirs).unwrap();
    }

    #[test]
    fn a_migration_edited_after_it_ran_is_caught_even_when_more_follow() {
        let (_dir, path) = database();
        left_by_an_older_keel(&path, 3);
        Store::open(&path).unwrap();
        let mut edited: Vec<&str> = MIGRATIONS.to_vec();
        let changed = format!("{}\n-- tidied up", MIGRATIONS[1]);
        edited[1] = &changed;
        edited.push("create table one_more (id text primary key);");

        assert!(matches!(
            Store::open_with(&path, &edited),
            Err(Error::FromAnotherKeel { number: 2 })
        ));
        assert_eq!(version(&path), MIGRATIONS.len() as i64);
    }

    #[test]
    fn a_migration_a_build_without_hashes_ran_is_taken_as_this_builds() {
        let (_dir, path) = database();
        Store::open_with(&path, &MIGRATIONS[..MIGRATIONS.len() - 1]).unwrap();
        // A Keel from before the hashes runs the last one: it counts it and records nothing.
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATIONS[MIGRATIONS.len() - 1])
                .unwrap();
            conn.pragma_update(None, "user_version", MIGRATIONS.len() as i64)
                .unwrap();
        }

        Store::open(&path).unwrap();

        let conn = Connection::open(&path).unwrap();
        assert_eq!(recorded(&conn).unwrap().len(), MIGRATIONS.len());
    }

    #[test]
    fn a_setting_can_be_read_from_a_database_this_build_refuses() {
        let (_dir, path) = database();
        let mut newer = MIGRATIONS.to_vec();
        newer.push("create table from_a_newer_keel (id text primary key);");
        made_by_hand(&path, &newer, newer.len(), false);
        Connection::open(&path)
            .unwrap()
            .execute(
                "insert into setting (key, value) values ('language', 'es')",
                [],
            )
            .unwrap();
        let before = shape(&path);
        assert_eq!(before.journal, "delete");

        assert_eq!(saved_setting(&path, "language").as_deref(), Some("es"));
        assert_eq!(saved_setting(&path, "nothing"), None);
        assert_eq!(shape(&path), before);
        assert_eq!(
            saved_setting(&path.with_file_name("none.db"), "language"),
            None
        );
        assert!(!path.with_file_name("none.db").exists());
    }
}
