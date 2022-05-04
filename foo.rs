#![cfg(unix)]
use std::collections::hash_map::{DefaultHasher, HashMap};
use std::ffi::{OsStr, OsString};
use std::hash::{Hash, Hasher};
use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::str::{from_utf8, FromStr};
use std::sync::{Arc, Mutex};
use std::{
	env, fs, iter,
	net::{self, IpAddr, SocketAddr},
	ops, thread, time,
};

use anyhow::anyhow;

trait Localhost {
	const LOCALHOST: Self;
}

impl Localhost for net::Ipv6Addr {
	const LOCALHOST: Self = Self::LOCALHOST;
}

impl Localhost for net::Ipv4Addr {
	const LOCALHOST: Self = Self::LOCALHOST;
}

macro_rules! git_repo {
	(
		|$iter:ident| $ex:expr,
		$(#[$meta:meta])?
		struct $name:ident <$a:lifetime> {
			$($key:ident : $value:ty),+$(,)?
		}
	) => {
		$(#[$meta])?
		struct $name<$a> {$($key : $value),+}

		impl<$a> $name<$a> {
			fn new($iter: &mut impl Iterator<Item = &$a [u8]>) -> Option<Self> {
				$(let $key = $ex);+;
				Some($name {$($key),+})
			}
		}
	};
}

const RECORD_SEPARATOR: u8 = 30;
const FIELD_SEPARATOR: u8 = 31;

#[derive(Debug)]
struct Response<'a> {
	id: &'a [u8],
	data: Option<GitRepo<'a>>,
}

git_repo! {
	|iter| iter.next().and_then(ConvertField::from_field)?,

	#[derive(Debug)]
	struct GitRepo<'a> {
		absolute_path: &'a [u8],
		commit_hash: &'a str,
		branch: Option<&'a [u8]>,
		upstream_branch: Option<&'a [u8]>,
		remote_name: Option<&'a [u8]>,
		remote_url: Option<&'a [u8]>,
		repository_state: Option<&'a [u8]>,
		files: u64,
		staged_changes: u64,
		unstaged_changes: u64,
		conflicted_changes: u64,
		untracked_files: u64,
		ahead: u64,
		behind: u64,
		stashes: u64,
		last_tag: Option<&'a [u8]>,
		unstaged_deleted_files: u64,
		staged_new_files: u64,
		staged_deleted_files: u64,
		push_remote_name: Option<&'a [u8]>,
		push_remote_url: Option<&'a [u8]>,
		ahead_push_remote: Option<&'a [u8]>,
		behind_push_remote: Option<&'a [u8]>,
		skip_worktree_files: u64,
		assume_unchanged_files: u64,
		commit_message_encoding: Option<&'a str>,
		commit_message: Option<&'a [u8]>,
	}
}

const BRANCH_SYMBOL: char = '\u{e0a0}';
const REF_SYMBOL: char = '\u{27a6}';
const DIRTY_SYMBOL: char = '\u{25cf}';
const STAGED_SYMBOL: char = '\u{271a}';

fn format_git_repo(repo: GitRepo<'_>) -> String {
	let mut res = String::new();

	let dirty = repo.staged_changes
		+ repo.unstaged_changes
		+ repo.conflicted_changes
		+ repo.untracked_files
		> 0;

	if dirty {
		res.push('@');
	}

	if let Some(branch) = repo.branch {
		res.push(BRANCH_SYMBOL);
		res.push(' ');
		res.push_str(&String::from_utf8_lossy(branch));
	} else {
		res.push(REF_SYMBOL);
		res.push(' ');

		let hash = repo.commit_hash.chars().take(8);
		for c in hash {
			res.push(c)
		}
	}

	if repo.ahead > 0 {
		res.push_str(&format!("⇡ {}", repo.ahead));
	}

	if repo.behind > 0 {
		res.push_str(&format!("⇣ {}", repo.behind));
	}

	// no untrack
	if repo.staged_changes > 0
		|| repo.unstaged_changes > 0
		|| repo.conflicted_changes > 0
	{
		res.push(' ')
	}

	if repo.unstaged_changes > 0 {
		res.push(DIRTY_SYMBOL);
	}
	if repo.staged_changes > 0 {
		res.push(STAGED_SYMBOL);
	}
	if repo.conflicted_changes > 0 {
		res.push_str("!!");
	}

	res
}

pub trait ConvertField<'a> {
	fn from_field(field: &'a [u8]) -> Option<Self>
	where
		Self: Sized;
}

impl<'a> ConvertField<'a> for &'a [u8] {
	#[inline]
	fn from_field(field: &'a [u8]) -> Option<Self> {
		Some(field)
	}
}

impl<'a> ConvertField<'a> for &'a str {
	#[inline]
	fn from_field(field: &'a [u8]) -> Option<Self> {
		from_utf8(field).ok()
	}
}

impl<'a> ConvertField<'a> for u64 {
	#[inline]
	fn from_field(field: &'a [u8]) -> Option<Self> {
		from_utf8(field).ok().and_then(|s| u64::from_str(s).ok())
	}
}

impl<'a, T: ConvertField<'a>> ConvertField<'a> for Option<T> {
	#[inline]
	fn from_field(field: &'a [u8]) -> Option<Self> {
		if field.is_empty() {
			Some(None)
		} else {
			Some(T::from_field(field))
		}
	}
}

const ONE_MS: time::Duration = time::Duration::from_millis(1);

fn sleep_little() {
	thread::sleep(ONE_MS);
}

#[inline]
fn is_separator(b: &u8) -> bool {
	let b = *b;
	b == FIELD_SEPARATOR || b == RECORD_SEPARATOR
}

fn parse_id(bytes: &[u8]) -> Option<(&[u8], Option<&[u8]>)> {
	let mut records = bytes.splitn(3, is_separator);
	let id = records.next()?;
	let is_git_repo = records.next()? != [b'0'];

	if !is_git_repo {
		return Some((id, None));
	}
	Some((id, Some(records.next()?)))
}

fn make_request<H: Hasher + Default>(
	path: &[u8],
	read_git_index: bool,
) -> (u64, Vec<u8>) {
	let hash = {
		let mut hasher: H = Default::default();
		hasher.write(path);
		hasher.finish()
	};

	let res: Vec<u8> = format!("{hash:x}")
		.as_bytes()
		.iter()
		.chain(&[FIELD_SEPARATOR])
		.chain(path)
		.chain(&[FIELD_SEPARATOR])
		.chain(&[if read_git_index { b'0' } else { b'1' }])
		.chain(&[RECORD_SEPARATOR])
		.copied()
		.collect();

	(hash, res)
}

fn server(
	host: net::SocketAddr,
	bin_path: PathBuf,
	args: impl IntoIterator<Item = impl AsRef<OsStr>>,
) -> io::Result<()> {
	let in_queue = Arc::new(Mutex::new(HashMap::<u64, net::SocketAddr>::new()));
	let in_queue_clone = in_queue.clone();

	let process_name = bin_path;
	let mut process = Command::new(process_name)
		.args(args)
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::inherit())
		.spawn()?;

	let mut stdin = process.stdin.take().unwrap();
	let mut stdout = BufReader::new(process.stdout.take().unwrap());

	let socket = Arc::new(net::UdpSocket::bind(host)?);
	socket.set_read_timeout(Some(time::Duration::from_millis(1000)))?;
	let socket_clone = socket.clone();

	let sender = thread::spawn(move || {
		let socket = socket_clone;
		let in_queue = in_queue_clone;
		let mut buf = Vec::new();

		while let Ok(len) = stdout.read_until(RECORD_SEPARATOR, &mut buf) {
			let chunk = &buf[..len];

			let (id, rest) = match parse_id(chunk) {
				Some(x) => x,
				None => {
					if chunk.last() == Some(&RECORD_SEPARATOR) {
						eprintln!("Unknown response: {chunk:x?}");
						continue;
					} else {
						eprintln!("{}", String::from_utf8_lossy(chunk));
						break;
					}
				}
			};

			let id = from_utf8(id)
				.ok()
				.and_then(|s| u64::from_str_radix(s, 16).ok());

			if let Some(id) = id {
				let mut in_queue = in_queue.lock().unwrap();

				if let Some(origin) = in_queue.remove(&id) {
					if let Err(e) = socket
						.send_to(rest.unwrap_or(&[RECORD_SEPARATOR]), origin)
					{
						eprintln!("Sending error: {origin} -> {e:?}");
						continue;
					}
				}
			}

			buf.clear();
		}
	});

	let receiver = thread::spawn(move || -> io::Result<()> {
		let mut buf = [0; 4096];

		while process.try_wait().ok().flatten().is_none() {
			let (bytes, origin) = match socket.recv_from(&mut buf) {
				Ok(x) => x,
				Err(e) => match e.kind() {
					io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => {
						sleep_little();
						continue;
					}
					_ => return Err(e),
				},
			};
			if origin.ip() != host.ip() {
				continue;
			}

			let (hash, request) =
				make_request::<DefaultHasher>(&buf[..bytes], true);

			stdin.write_all(&request)?;

			{
				in_queue.lock().unwrap().insert(hash, origin);
			}
		}
		Ok(())
	});

	sender.join().ok();
	receiver.join().ok();
	Ok(())
}

fn client(
	from: impl net::ToSocketAddrs,
	cwd: PathBuf,
	host: net::SocketAddr,
) -> io::Result<()> {
	let socket = net::UdpSocket::bind(from)?;
	socket.connect(host)?;

	let mut buf = [0; 4096];

	socket.send(cwd.into_os_string().as_bytes())?;
	socket.set_read_timeout(Some(time::Duration::from_millis(500)))?;

	let bytes = socket.recv(&mut buf)?;

	let mut words = buf[0..bytes].split(is_separator);
	let repo = GitRepo::new(&mut words);

	if let Some(repo) = repo {
		println!("{}", format_git_repo(repo));
	} else {
	}

	Ok(())
}

const USAGE: &str = r#"
Usage:
	foo [options] [subcommand] [... subcommand args]

Options:
	--port=server:client
		Set UDP port for server and client.
		Client port can be a number range using hyphen(-).
		Defaults to 8080 for server, 8081-8090 for client.

		Example:
			foo --port=8080:8081-8090 client .

Subcommands:
	client [dir]
		Get git status for [dir].

	server [gitstatusd path] [... gitstatusd options]
		Run [gitstatusd path] and passes [... gitstatusd options] to it.
"#;

#[derive(Debug)]
struct Cli {
	server_port: u16,
	client_port: ops::RangeInclusive<u16>,
	command: CliCommand,
}

#[derive(Debug)]
enum CliCommand {
	Server {
		gitstatusd_path: PathBuf,
		gitstatusd_options: Vec<OsString>,
	},
	Client {
		cwd: PathBuf,
	},
}

fn parse_args() -> Option<Cli> {
	#[derive(Clone, Copy)]
	enum Subcommand {
		Server,
		Client,
	}

	let mut args = env::args_os().skip(1);

	let mut server_port = 8080u16;
	let mut client_port_range = 8081..=8090u16;
	let mut subcommand: Option<Subcommand> = None;

	for _ in 1..=2 {
		let next = args.next()?;
		let next = next.to_string_lossy();

		if let Some(port) = next.strip_prefix("--port=") {
			let mut a = port.splitn(2, ':');
			server_port = a.next().and_then(|a| a.parse().ok())?;

			let mut a = a.next()?.splitn(2, '-');
			let client_start = a.next().and_then(|a| a.parse().ok())?;
			let client_end = match a.next() {
				Some(end) => end.parse().ok()?,
				None => client_start,
			};

			client_port_range = if client_start > client_end {
				client_end..=client_start
			} else {
				client_start..=client_end
			};

			continue;
		} else {
			subcommand = Some(match next.to_ascii_lowercase().as_str() {
				"server" => Subcommand::Server,
				"client" => Subcommand::Client,
				_ => return None,
			});
			break;
		}
	}

	let path = PathBuf::from(args.next()?);

	let cli_command = match subcommand? {
		Subcommand::Server => {
			let options = args.collect();
			CliCommand::Server {
				gitstatusd_path: path,
				gitstatusd_options: options,
			}
		}
		Subcommand::Client => CliCommand::Client { cwd: path },
	};

	let cli = Cli {
		server_port,
		client_port: client_port_range,
		command: cli_command,
	};

	Some(cli)
}

fn main() -> anyhow::Result<()> {
	let args = parse_args().ok_or_else(|| anyhow!(USAGE))?;
	let localhost = IpAddr::V6(Localhost::LOCALHOST);

	let host = SocketAddr::new(localhost, args.server_port);
	let from: Vec<SocketAddr> = args
		.client_port
		.map(|port| SocketAddr::new(localhost, port))
		.collect();

	match args.command {
		CliCommand::Server {
			gitstatusd_path,
			gitstatusd_options,
		} => server(host, gitstatusd_path, gitstatusd_options),
		CliCommand::Client { cwd } => {
			client(from.as_slice(), fs::canonicalize(cwd)?, host)
		}
	}?;

	Ok(())
}
