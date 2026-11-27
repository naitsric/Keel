//! The way out for a check a person repeats after allowing a domain it lacked. A Seatbelt profile
//! can't hold a command to a list of domains, so the check's sandbox opens one port on localhost,
//! this proxy's (see `settings::with_proxy`), and the proxy, outside the sandbox, connects only to
//! the domains the check may reach, and never to this Mac or to an address of a local network. It
//! asks for a password that only that check is given, so another sandbox with localhost open can't
//! borrow it, and it goes when the check ends.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, TcpListener, TcpStream, ToSocketAddrs, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use base64::Engine as _;

use crate::{locked, network};

/// The most connections at once, counting those of every proxy there is: they all hold the app's
/// descriptors, and each connection holds two, the command's end and the domain's. That's 96 of
/// the 256 macOS gives an app unless it asks for more, which leaves Keel its own files, and is as
/// many as pnpm opens to one registry, the most of the package managers. One more waits its turn
/// without being answered, which a command sees as a slow connection and not as a failure.
const MOST_AT_ONCE: usize = 48;
const MOST_HEAD_BYTES: usize = 32 * 1024;
/// For a request to arrive, and for the domain it names to answer.
const TIMEOUT: Duration = Duration::from_secs(30);
const USER: &str = "keel";

type Connect = dyn Fn(&str, u16) -> io::Result<TcpStream> + Send + Sync;

/// How a check's proxy connects to a domain it lets through, for `Config::check_connects`. The
/// app has none, and connects as `connect_out` does. A test gives one that leads to a server of
/// its own, so that it needs no internet.
#[derive(Clone)]
pub struct Connector(Arc<Connect>);

impl Connector {
    pub fn new(
        connect: impl Fn(&str, u16) -> io::Result<TcpStream> + Send + Sync + 'static,
    ) -> Self {
        Self(Arc::new(connect))
    }
}

impl std::fmt::Debug for Connector {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Connector")
    }
}

/// What stands in what Keel keeps for a proxy's password.
const MASK: &str = "***";
const PASSWORD_CHARS: usize = 32;
/// How `keel:` and a password start once encoded for a `Proxy-Authorization` header, which says
/// no more than that it's this user's, and how long the whole of it is.
const ENCODED_START: &str = "a2VlbD";
const ENCODED_CHARS: usize = 52;

/// `text` without any proxy's password, as it is and as a header carries it. A check has it in
/// its environment, so what it prints can have it: `echo $HTTPS_PROXY` does, and so does a tool
/// that shows the requests it makes. What Keel keeps of that, shows and hands to the next agent
/// doesn't. `secrets` are those of the proxy the check had (see `Proxy::secrets`), which go
/// wherever they are. Beyond those it's told by its shape after its user, so it's found for a
/// check the app found already running too, whose proxy is gone.
pub(crate) fn masked(text: &str, secrets: &[String]) -> String {
    let mut known = text.to_owned();
    for secret in secrets {
        known = known.replace(secret, MASK);
    }
    let text = known.as_str();
    let plain = format!("{USER}:");
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    loop {
        let found = [(plain.as_str(), true), (ENCODED_START, false)]
            .into_iter()
            .filter_map(|(start, is_plain)| Some((rest.find(start)?, start, is_plain)))
            .min_by_key(|(at, ..)| *at);
        let Some((at, start, is_plain)) = found else {
            out.push_str(rest);
            return out;
        };
        let after = &rest[at + start.len()..];
        let secret = if is_plain {
            let hex = |b: u8| b.is_ascii_digit() || (b'a'..=b'f').contains(&b);
            let bytes = after.as_bytes();
            (bytes.len() >= PASSWORD_CHARS && bytes[..PASSWORD_CHARS].iter().all(|b| hex(*b)))
                .then_some((start.len(), PASSWORD_CHARS))
        } else {
            let encoded = |b: u8| b.is_ascii_alphanumeric() || b"+/=".contains(&b);
            let length = ENCODED_CHARS - start.len();
            let bytes = after.as_bytes();
            (bytes.len() >= length && bytes[..length].iter().all(|b| encoded(*b)))
                .then_some((0, ENCODED_CHARS))
        };
        match secret {
            Some((kept, hidden)) => {
                out.push_str(&rest[..at + kept]);
                out.push_str(MASK);
                rest = &rest[at + kept + hidden..];
            }
            None => {
                out.push_str(&rest[..at + start.len()]);
                rest = after;
            }
        }
    }
}

/// How many connections are open, of at most so many. One for every proxy of the app: see
/// `MOST_AT_ONCE`.
struct Places {
    taken: AtomicUsize,
    most: usize,
}

static PLACES: Places = Places {
    taken: AtomicUsize::new(0),
    most: MOST_AT_ONCE,
};

/// A connection's place among those open, given back when dropped.
struct Place(&'static Places);

impl Places {
    /// A place for one more connection, or `None` while there are as many as there may be.
    fn take(&'static self) -> Option<Place> {
        let mut taken = self.taken.load(Ordering::SeqCst);
        while taken < self.most {
            let now = taken + 1;
            match self
                .taken
                .compare_exchange(taken, now, Ordering::SeqCst, Ordering::SeqCst)
            {
                Ok(_) => return Some(Place(self)),
                Err(found) => taken = found,
            }
        }
        None
    }
}

impl Drop for Place {
    fn drop(&mut self) {
        self.0.taken.fetch_sub(1, Ordering::SeqCst);
    }
}

struct Shared {
    domains: Vec<String>,
    password: String,
    connect: Box<Connect>,
    places: &'static Places,
    stopped: AtomicBool,
    /// The sockets of the connections under way, to close them when the proxy goes.
    open: Mutex<HashMap<u64, Vec<Arc<TcpStream>>>>,
}

pub(crate) struct Proxy {
    port: u16,
    shared: Arc<Shared>,
    accepting: Option<JoinHandle<()>>,
}

impl Proxy {
    /// A proxy on a port of localhost the system picks, for these domains. It connects to them
    /// as `connect_out` does, unless it's given another way.
    pub(crate) fn start(domains: &[String], connect: Option<&Connector>) -> io::Result<Self> {
        let connect: Box<Connect> = match connect {
            Some(Connector(connect)) => {
                let connect = connect.clone();
                Box::new(move |host, port| connect(host, port))
            }
            None => Box::new(connect_out),
        };
        Self::start_with(domains, connect)
    }

    fn start_with(domains: &[String], connect: Box<Connect>) -> io::Result<Self> {
        Self::start_among(domains, connect, &PLACES)
    }

    fn start_among(
        domains: &[String],
        connect: Box<Connect>,
        places: &'static Places,
    ) -> io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let shared = Arc::new(Shared {
            domains: domains.to_vec(),
            password: uuid::Uuid::new_v4().simple().to_string(),
            connect,
            places,
            stopped: AtomicBool::new(false),
            open: Mutex::default(),
        });
        let accepting = {
            let shared = shared.clone();
            std::thread::Builder::new().spawn(move || accept(&listener, &shared))?
        };
        Ok(Self {
            port,
            shared,
            accepting: Some(accepting),
        })
    }

    pub(crate) fn port(&self) -> u16 {
        self.port
    }

    /// Its password, alone and as a `Proxy-Authorization` header carries it: what `masked` takes
    /// out of what the check printed.
    pub(crate) fn secrets(&self) -> Vec<String> {
        let password = &self.shared.password;
        let pair = format!("{USER}:{password}");
        vec![
            password.clone(),
            base64::engine::general_purpose::STANDARD.encode(pair),
        ]
    }

    /// The variables that send a command's downloads through it. What's on localhost, such as a
    /// server a test starts, isn't a download. Java reads none of these and gets nothing in their
    /// place: it prints the options it's given, password and all, won't send a password to open a
    /// tunnel unless told to, and no check reaches a repository of Maven's or Gradle's anyway,
    /// since the only domains a check is ever allowed are those of the tools Keel can tell failed
    /// offline (see `network::failed_offline`). So Java stays offline here, as the check before.
    pub(crate) fn env(&self) -> Vec<(String, String)> {
        let url = format!(
            "http://{USER}:{}@127.0.0.1:{}",
            self.shared.password, self.port
        );
        let proxied = ["HTTP_PROXY", "HTTPS_PROXY", "http_proxy", "https_proxy"]
            .map(|name| (name.to_owned(), url.clone()));
        let direct = ["NO_PROXY", "no_proxy"]
            .map(|name| (name.to_owned(), "localhost,127.0.0.1,::1".to_owned()));
        proxied.into_iter().chain(direct).collect()
    }
}

impl Drop for Proxy {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::SeqCst);
        // The port closes with the thread that listens on it.
        if let Some(accepting) = self.accepting.take() {
            let _ = accepting.join();
        }
        for socket in locked(&self.shared.open).values().flatten() {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }
}

fn accept(listener: &TcpListener, shared: &Arc<Shared>) {
    let mut next = 0u64;
    while !shared.stopped.load(Ordering::SeqCst) {
        // Taken before the connection is, so that one past the most there may be holds no
        // descriptor of the app's: it waits in the system's queue until another ends.
        let accepted = shared
            .places
            .take()
            .and_then(|place| Some((listener.accept().ok()?.0, place)));
        let Some((client, place)) = accepted else {
            std::thread::sleep(Duration::from_millis(20));
            continue;
        };
        let client = Arc::new(client);
        let id = next;
        next += 1;
        locked(&shared.open).insert(id, vec![client.clone()]);
        let serving = shared.clone();
        let spawned = std::thread::Builder::new().spawn(move || {
            // A connection that breaks is the command's to report.
            let _ = serve(&serving, id, &client);
            let _ = client.shutdown(Shutdown::Both);
            locked(&serving.open).remove(&id);
            // Once both its ends are closed.
            drop(client);
            drop(place);
        });
        if spawned.is_err() {
            locked(&shared.open).remove(&id);
        }
    }
}

/// A request's first line and headers.
struct Head<'a> {
    method: &'a str,
    target: &'a str,
    headers: Vec<(&'a str, &'a str)>,
}

impl<'a> Head<'a> {
    fn parse(text: &'a str) -> Option<Self> {
        let mut lines = text.split("\r\n");
        let mut first = lines.next()?.split(' ');
        let (method, target) = (first.next()?, first.next()?);
        let headers = lines
            .filter(|line| !line.is_empty())
            .map(|line| line.split_once(':').map(|(n, v)| (n, v.trim())))
            .collect::<Option<_>>()?;
        Some(Self {
            method,
            target,
            headers,
        })
    }

    fn header(&self, name: &str) -> Option<&'a str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, value)| *value)
    }
}

/// A request's head, and what came after it in the same reads.
fn read_head(mut client: &TcpStream) -> io::Result<(String, Vec<u8>)> {
    let mut read = Vec::new();
    let mut buffer = [0u8; 4096];
    loop {
        if let Some(end) = read.windows(4).position(|w| w == b"\r\n\r\n") {
            let rest = read.split_off(end + 4);
            let head = String::from_utf8(read).map_err(io::Error::other)?;
            return Ok((head, rest));
        }
        let count = client.read(&mut buffer)?;
        if count == 0 || read.len() + count > MOST_HEAD_BYTES {
            return Err(io::ErrorKind::InvalidData.into());
        }
        read.extend_from_slice(&buffer[..count]);
    }
}

fn answer(mut client: &TcpStream, status: &str) -> io::Result<()> {
    let ask = if status.starts_with("407") {
        "Proxy-Authenticate: Basic realm=\"keel\"\r\n"
    } else {
        ""
    };
    write!(
        client,
        "HTTP/1.1 {status}\r\n{ask}Content-Length: 0\r\nConnection: close\r\n\r\n"
    )
}

/// The host and port of `host:port`, or of a host alone where there's a port to assume.
fn host_and_port(authority: &str, assumed: Option<u16>) -> Option<(String, u16)> {
    let (host, port) = match authority.split_once(':') {
        Some((host, port)) => (host, port.parse().ok()?),
        None => (authority, assumed?),
    };
    let host = host.to_ascii_lowercase();
    // Nothing but a host name: no user, brackets or wildcard.
    (network::is_domain(&host) && !host.starts_with('*')).then_some((host, port))
}

fn serve(shared: &Shared, id: u64, client: &Arc<TcpStream>) -> io::Result<()> {
    // As accepted from a listener that doesn't block, it may not either.
    client.set_nonblocking(false)?;
    client.set_read_timeout(Some(TIMEOUT))?;
    let (text, rest) = read_head(client)?;
    let Some(head) = Head::parse(&text) else {
        return answer(client, "400 Bad Request");
    };
    let expected = format!(
        "Basic {}",
        base64::engine::general_purpose::STANDARD.encode(format!("{USER}:{}", shared.password))
    );
    if head.header("proxy-authorization") != Some(&expected) {
        return answer(client, "407 Proxy Authentication Required");
    }
    // `CONNECT host:port` opens a tunnel, as for HTTPS. Plain HTTP names the whole address.
    let tunnels = head.method == "CONNECT";
    let (authority, path) = if tunnels {
        (head.target, "")
    } else {
        let Some(address) = head.target.strip_prefix("http://") else {
            return answer(client, "400 Bad Request");
        };
        address.split_at(address.find(['/', '?']).unwrap_or(address.len()))
    };
    let Some((host, port)) = host_and_port(authority, (!tunnels).then_some(80)) else {
        return answer(client, "403 Forbidden");
    };
    if !network::covers(&shared.domains, &host) {
        return answer(client, "403 Forbidden");
    }
    let Ok(upstream) = (shared.connect)(&host, port) else {
        return answer(client, "502 Bad Gateway");
    };
    let upstream = Arc::new(upstream);
    {
        let mut open = locked(&shared.open);
        // Looked at while the list is held, so the proxy can't go without closing it.
        if shared.stopped.load(Ordering::SeqCst) {
            return Ok(());
        }
        if let Some(sockets) = open.get_mut(&id) {
            sockets.push(upstream.clone());
        }
    }
    client.set_read_timeout(None)?;
    if tunnels {
        (&**client).write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")?;
        (&*upstream).write_all(&rest)?;
        return tunnel(client, &upstream);
    }
    // One request a connection: a second one could name a host this one wasn't checked for.
    if head.header("transfer-encoding").is_some() {
        return answer(client, "501 Not Implemented");
    }
    let body: u64 = head
        .header("content-length")
        .and_then(|length| length.parse().ok())
        .unwrap_or(0);
    let slash = if path.starts_with('/') { "" } else { "/" };
    let mut request = format!(
        "{} {slash}{path} HTTP/1.1\r\nHost: {authority}\r\n",
        head.method
    );
    for (name, value) in &head.headers {
        let lower = name.to_ascii_lowercase();
        if !lower.starts_with("proxy-")
            && !["host", "connection", "keep-alive"].contains(&lower.as_str())
        {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
    }
    request.push_str("Connection: close\r\n\r\n");
    (&*upstream).write_all(request.as_bytes())?;
    let early = rest.len().min(usize::try_from(body).unwrap_or(usize::MAX));
    (&*upstream).write_all(&rest[..early])?;
    io::copy(&mut (&**client).take(body - early as u64), &mut &*upstream)?;
    io::copy(&mut &*upstream, &mut &**client)?;
    Ok(())
}

/// Copies both ways until each side is done sending.
fn tunnel(client: &Arc<TcpStream>, upstream: &Arc<TcpStream>) -> io::Result<()> {
    let sending = {
        let (client, upstream) = (client.clone(), upstream.clone());
        std::thread::Builder::new().spawn(move || {
            let _ = io::copy(&mut &*client, &mut &*upstream);
            let _ = upstream.shutdown(Shutdown::Write);
        })?
    };
    let received = io::copy(&mut &**upstream, &mut &**client);
    let _ = client.shutdown(Shutdown::Write);
    let _ = sending.join();
    received.map(|_| ())
}

/// Connects to a host at an address that's neither this Mac's nor a local network's: a name that
/// resolves to one would be a way to what the check's sandbox keeps closed, and to machines only
/// this Mac's network reaches.
fn connect_out(host: &str, port: u16) -> io::Result<TcpStream> {
    let mut failed = io::Error::from(io::ErrorKind::PermissionDenied);
    for address in (host, port).to_socket_addrs()? {
        if !is_elsewhere(address.ip()) {
            continue;
        }
        match TcpStream::connect_timeout(&address, TIMEOUT) {
            Ok(stream) => return Ok(stream),
            Err(error) => failed = error,
        }
    }
    Err(failed)
}

/// Whether an address is a machine's on the internet: not this Mac's, by loopback or by any address
/// of its own, nor one that only reaches the link it's on or a private network, such as a router
/// or another machine at home or at work. No project setting opens those to a check.
fn is_elsewhere(address: IpAddr) -> bool {
    is_elsewhere_given(address, |address| UdpSocket::bind((address, 0)).map(|_| ()))
}

/// `is_elsewhere`, with what listening on the address says. Only this Mac's own addresses can be
/// listened on, so an address is another machine's only when the system says it isn't one of
/// them. Anything else it says, such as having no descriptors left, tells nothing about the
/// address, and then it counts as this Mac's.
fn is_elsewhere_given(address: IpAddr, listen: impl Fn(IpAddr) -> io::Result<()>) -> bool {
    // An IPv4 address inside an IPv6 one is that IPv4 address, however it's written.
    let address = match address {
        IpAddr::V6(v6) => v6.to_ipv4().map_or(address, IpAddr::V4),
        IpAddr::V4(_) => address,
    };
    let special = match address {
        IpAddr::V4(v4) => {
            v4.octets()[0] == 0
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_multicast()
                || v4.is_broadcast()
                || v4.is_private()
                || is_shared(v4)
        }
        IpAddr::V6(v6) => {
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                // The old site-local addresses, which some networks still use.
                || (v6.segments()[0] & 0xffc0) == 0xfec0
                || (v6.segments()[0] & 0xfe00) == 0xfc00
        }
    };
    !special && listen(address).is_err_and(|error| error.kind() == io::ErrorKind::AddrNotAvailable)
}

/// 100.64.0.0/10, which carriers and private networks between machines use.
fn is_shared(address: Ipv4Addr) -> bool {
    let [first, second, ..] = address.octets();
    first == 100 && (second & 0xc0) == 64
}

#[cfg(test)]
mod tests {
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::sync::mpsc::{Receiver, channel};

    use super::*;

    /// Two connected ends, as the two of a connection. A pair needs no port, which the sandbox
    /// Keel's own tests may run in doesn't open, and what's served over one is the same.
    fn pair() -> (TcpStream, TcpStream) {
        let (one, other) = UnixStream::pair().unwrap();
        (
            TcpStream::from(OwnedFd::from(one)),
            TcpStream::from(OwnedFd::from(other)),
        )
    }

    /// A proxy's rules for these domains, which finds every one of them at a server that answers
    /// with `hello` and tells what it was asked, up to the blank line.
    fn serving(domains: &[&str]) -> (Arc<Shared>, Receiver<String>) {
        let (tell, heard) = channel();
        let connect = move |_: &str, _: u16| {
            let (near, far) = pair();
            let tell = tell.clone();
            std::thread::spawn(move || {
                let (head, _) = read_head(&far).unwrap();
                (&far)
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello")
                    .unwrap();
                tell.send(head).unwrap();
            });
            Ok(near)
        };
        let shared = Shared {
            domains: domains.iter().map(|d| (*d).to_owned()).collect(),
            password: "secret".into(),
            connect: Box::new(connect),
            places: &PLACES,
            stopped: AtomicBool::new(false),
            open: Mutex::default(),
        };
        (Arc::new(shared), heard)
    }

    fn authorization(password: &str) -> String {
        let pair = format!("{USER}:{password}");
        format!(
            "Proxy-Authorization: Basic {}\r\n",
            base64::engine::general_purpose::STANDARD.encode(pair)
        )
    }

    /// Everything that comes back for `request`.
    fn ask(shared: &Arc<Shared>, request: &str) -> String {
        let (mut mine, theirs) = pair();
        mine.write_all(request.as_bytes()).unwrap();
        let shared = shared.clone();
        let served = std::thread::spawn(move || {
            let theirs = Arc::new(theirs);
            let served = serve(&shared, 0, &theirs);
            let _ = theirs.shutdown(Shutdown::Both);
            served
        });
        mine.set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let mut said = String::new();
        // A tunnel stays open until this end is done sending too.
        let mut start = [0; 12];
        mine.read_exact(&mut start).unwrap();
        let _ = mine.shutdown(Shutdown::Write);
        said.push_str(std::str::from_utf8(&start).unwrap());
        let _ = mine.read_to_string(&mut said);
        served.join().unwrap().unwrap();
        said
    }

    #[test]
    fn what_keel_keeps_of_what_a_check_printed_has_no_proxy_password() {
        let password = uuid::Uuid::new_v4().simple().to_string();
        let encoded =
            base64::engine::general_purpose::STANDARD.encode(format!("{USER}:{password}"));
        assert_eq!(
            (password.len(), encoded.len()),
            (PASSWORD_CHARS, ENCODED_CHARS)
        );
        assert!(encoded.starts_with(ENCODED_START), "{encoded}");
        let printed = format!(
            "HTTPS_PROXY=http://keel:{password}@127.0.0.1:50123\n\
             > Proxy-Authorization: Basic {encoded}\r\n\
             twice: keel:{password} keel:{password}é"
        );
        // By its shape alone, as for a check whose proxy went with the app.
        let kept = masked(&printed, &[]);
        assert!(!kept.contains(&password), "{kept}");
        assert!(!kept.contains(&encoded), "{kept}");
        assert_eq!(
            kept,
            "HTTPS_PROXY=http://keel:***@127.0.0.1:50123\n\
             > Proxy-Authorization: Basic ***\r\n\
             twice: keel:*** keel:***é"
        );
        // Away from its user nothing tells it, so only the proxy's own secrets take it out.
        let alone = format!("the password is {password}, {password}");
        assert_eq!(masked(&alone, &[]), alone);
        let secrets = [password.clone(), encoded.clone()];
        assert_eq!(masked(&alone, &secrets), "the password is ***, ***");
        assert_eq!(masked(&printed, &secrets), kept);

        // Anything else stays as it was printed.
        for plain in [
            "",
            "keel:",
            "keel: a run",
            "keel:0123456789abcdef",
            "user keel:XYZ23456789abcdef0123456789abcdef",
            "a2VlbD",
            "a2VlbDshort==",
            "an á and a2VlbD then *** and keel:é",
        ] {
            assert_eq!(masked(plain, &[]), plain);
        }
    }

    #[test]
    fn it_tunnels_to_an_allowed_domain_and_to_no_other() {
        let (shared, heard) = serving(&["registry.example", "*.cdn.example"]);
        let auth = authorization("secret");
        for refused in [
            "other.example:443",
            "cdn.example:443",
            "registry.example.evil:443",
            "127.0.0.1:443",
            "localhost:443",
            "[::1]:443",
            "user@registry.example:443",
            "registry.example",
        ] {
            let said = ask(
                &shared,
                &format!("CONNECT {refused} HTTP/1.1\r\n{auth}\r\n"),
            );
            assert!(said.starts_with("HTTP/1.1 403"), "{refused}: {said}");
        }
        assert!(heard.try_recv().is_err(), "none of those was connected to");

        let said = ask(
            &shared,
            &format!(
                "CONNECT A.CDN.example:443 HTTP/1.1\r\n{auth}\r\nGET / HTTP/1.1\r\nX: y\r\n\r\n"
            ),
        );

        assert!(
            said.starts_with("HTTP/1.1 200 Connection established"),
            "{said}"
        );
        assert!(said.ends_with("hello"), "{said}");
        assert_eq!(heard.recv().unwrap(), "GET / HTTP/1.1\r\nX: y\r\n\r\n");
    }

    #[test]
    fn it_passes_on_plain_http_to_an_allowed_domain_without_its_password() {
        let (shared, heard) = serving(&["registry.example"]);
        let auth = authorization("secret");
        let said = ask(
            &shared,
            &format!("GET http://other.example/x HTTP/1.1\r\n{auth}\r\n"),
        );
        assert!(said.starts_with("HTTP/1.1 403"), "{said}");

        let said = ask(
            &shared,
            &format!(
                "GET http://registry.example/next?x=1 HTTP/1.1\r\nHost: other.example\r\n{auth}Accept: */*\r\nConnection: keep-alive\r\n\r\n"
            ),
        );

        assert!(said.ends_with("hello"), "{said}");
        assert_eq!(
            heard.recv().unwrap(),
            "GET /next?x=1 HTTP/1.1\r\nHost: registry.example\r\nAccept: */*\r\nConnection: close\r\n\r\n"
        );
    }

    #[test]
    fn without_the_password_it_connects_nowhere() {
        let (shared, heard) = serving(&["registry.example"]);
        for auth in [String::new(), authorization("guess")] {
            let said = ask(
                &shared,
                &format!("CONNECT registry.example:443 HTTP/1.1\r\n{auth}\r\n"),
            );
            assert!(said.starts_with("HTTP/1.1 407"), "{said}");
        }
        assert!(heard.try_recv().is_err());
    }

    #[test]
    fn it_goes_with_the_check_and_closes_what_was_open() {
        keel_harness::needs!(sockets);
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let there = listener.local_addr().unwrap().port();
        let proxy = Proxy::start_with(
            &["registry.example".to_owned()],
            Box::new(move |_, _| TcpStream::connect(("127.0.0.1", there))),
        )
        .unwrap();
        let port = proxy.port();
        let auth = authorization(&proxy.shared.password);
        assert_eq!(proxy.secrets()[0], proxy.shared.password);
        assert_eq!(
            format!("Proxy-Authorization: Basic {}\r\n", proxy.secrets()[1]),
            auth
        );
        assert_eq!(
            proxy.env()[0].1,
            format!("http://keel:{}@127.0.0.1:{port}", proxy.shared.password)
        );
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        write!(
            stream,
            "CONNECT registry.example:443 HTTP/1.1\r\n{auth}\r\n"
        )
        .unwrap();
        let (_upstream, _) = listener.accept().unwrap();
        read_head(&stream).unwrap();

        drop(proxy);

        assert_eq!(stream.read(&mut [0; 1]).unwrap_or(0), 0);
        // Nothing of it answers on its port any more. Something else may: the port is free for
        // whatever asks for one next, another test included, so only what it would say counts.
        if let Ok(mut late) = TcpStream::connect(("127.0.0.1", port)) {
            late.set_read_timeout(Some(Duration::from_millis(500)))
                .unwrap();
            let _ = write!(late, "CONNECT registry.example:443 HTTP/1.1\r\n{auth}\r\n");
            let mut said = Vec::new();
            let _ = late.read_to_end(&mut said);
            let said = String::from_utf8_lossy(&said);
            assert!(!said.starts_with("HTTP/1.1 200"), "{said}");
        }
    }

    /// What the system says of listening on an address that isn't this Mac's.
    fn not_mine(_: IpAddr) -> io::Result<()> {
        Err(io::ErrorKind::AddrNotAvailable.into())
    }

    #[test]
    fn it_never_connects_to_this_mac() {
        for here in [
            "127.0.0.1",
            "127.9.9.9",
            "0.0.0.0",
            "169.254.169.254",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "::",
            "::ffff:127.0.0.1",
            "::127.0.0.1",
            "fe80::1",
        ] {
            assert!(!is_elsewhere(here.parse().unwrap()), "{here}");
            // Whatever listening on it says.
            assert!(
                !is_elsewhere_given(here.parse().unwrap(), not_mine),
                "{here}"
            );
        }
        // An address of its own on a network, which it can listen on.
        assert!(!is_elsewhere_given(
            "93.184.216.34".parse().unwrap(),
            |_| { Ok(()) }
        ));
        keel_harness::needs!(sockets);
        for there in ["93.184.216.34", "2606:2800:220:1::1"] {
            assert!(is_elsewhere(there.parse().unwrap()), "{there}");
        }
        assert!(connect_out("localhost", 9).is_err());
    }

    #[test]
    fn it_never_connects_to_an_address_of_a_private_network() {
        for private in [
            "10.0.0.1",
            "10.255.255.255",
            "172.16.0.1",
            "172.31.255.255",
            "192.168.1.1",
            "100.64.0.1",
            "100.127.255.255",
            "fc00::1",
            "fd12:3456:789a::1",
            "::ffff:192.168.1.1",
            "::ffff:10.0.0.1",
            "::192.168.1.1",
            "fec0::1",
            "feff::1",
        ] {
            assert!(
                !is_elsewhere_given(private.parse().unwrap(), not_mine),
                "{private}"
            );
            assert!(!is_elsewhere(private.parse().unwrap()), "{private}");
        }
        // The ones next to those are the internet's.
        for public in [
            "9.255.255.255",
            "11.0.0.1",
            "172.15.255.255",
            "172.32.0.1",
            "192.167.255.255",
            "192.169.0.1",
            "100.63.255.255",
            "100.128.0.1",
            "fe00::1",
            "2606:2800:220:1::1",
        ] {
            assert!(
                is_elsewhere_given(public.parse().unwrap(), not_mine),
                "{public}"
            );
        }
    }

    /// Only "that address isn't here" says an address is another machine's.
    #[test]
    fn an_address_it_cannot_tell_is_another_machines_counts_as_this_macs() {
        let there: IpAddr = "93.184.216.34".parse().unwrap();
        assert!(is_elsewhere_given(there, not_mine));
        let no_descriptors_left =
            |_: IpAddr| -> io::Result<()> { Err(io::Error::from_raw_os_error(24)) };
        assert_eq!(
            no_descriptors_left(there).unwrap_err().to_string(),
            "Too many open files (os error 24)"
        );
        assert!(!is_elsewhere_given(there, no_descriptors_left));
        for kind in [
            io::ErrorKind::PermissionDenied,
            io::ErrorKind::AddrInUse,
            io::ErrorKind::OutOfMemory,
            io::ErrorKind::Other,
        ] {
            assert!(!is_elsewhere_given(there, |_| Err(kind.into())), "{kind:?}");
        }
    }

    #[test]
    fn every_proxy_shares_the_places_for_connections_and_gives_them_back() {
        let places: &'static Places = Box::leak(Box::new(Places {
            taken: AtomicUsize::new(0),
            most: 2,
        }));
        let (first, second) = (places.take().unwrap(), places.take().unwrap());
        assert!(places.take().is_none(), "two are as many as there may be");
        drop(first);
        let third = places.take().unwrap();
        assert!(places.take().is_none());
        drop((second, third));
        assert_eq!(places.taken.load(Ordering::SeqCst), 0);
        // Each connection holds two descriptors, and the app has 256 unless it asks for more.
        assert!(PLACES.most * 2 <= 128, "{}", PLACES.most);
    }

    /// One proxy at its most leaves no place in another, and a connection past the most waits
    /// instead of being refused.
    #[test]
    fn a_connection_past_the_most_waits_for_another_to_end() {
        keel_harness::needs!(sockets);
        let places: &'static Places = Box::leak(Box::new(Places {
            taken: AtomicUsize::new(0),
            most: 1,
        }));
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let there = listener.local_addr().unwrap().port();
        let start = || {
            Proxy::start_among(
                &["registry.example".to_owned()],
                Box::new(move |_, _| TcpStream::connect(("127.0.0.1", there))),
                places,
            )
            .unwrap()
        };
        let (one, other) = (start(), start());
        let connect = |proxy: &Proxy| {
            let mut stream = TcpStream::connect(("127.0.0.1", proxy.port())).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_millis(300)))
                .unwrap();
            let auth = authorization(&proxy.shared.password);
            write!(
                stream,
                "CONNECT registry.example:443 HTTP/1.1\r\n{auth}\r\n"
            )
            .unwrap();
            stream
        };
        let first = connect(&one);
        let (upstream, _) = listener.accept().unwrap();
        read_head(&first).unwrap();
        assert_eq!(places.taken.load(Ordering::SeqCst), 1);

        // The other proxy's, while the first is open: nobody answers it yet.
        let second = connect(&other);
        assert!(read_head(&second).is_err(), "it waits");

        // Both ends of the first are done.
        drop((first, upstream));
        second
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let (_upstream, _) = listener.accept().unwrap();
        let (head, _) = read_head(&second).unwrap();
        assert!(head.starts_with("HTTP/1.1 200"), "{head}");
        drop((one, other));
    }
}
