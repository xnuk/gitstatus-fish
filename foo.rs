use std::collections::hash_map::{DefaultHasher, HashMap};
use std::ffi::OsStr;
use std::hash::{Hash, Hasher};
use std::io::{self, BufRead, BufReader, Write};
use std::process::{Child, Command, Stdio};
use std::str::{from_utf8, FromStr};
use std::sync::{Arc, Mutex};
use std::{env, iter, net, thread};

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
		if field.len() == 0 {
			None
		} else {
			Some(T::from_field(field))
		}
	}
}

fn parse_response<'a>(bytes: &'a [u8]) -> Option<Response<'a>> {
	let mut records =
		bytes.split(|&b| b == FIELD_SEPARATOR || b == RECORD_SEPARATOR);
	let id = records.next()?;
	let is_git_repo = records.next()? != [0];

	if !is_git_repo {
		return Some(Response { id, data: None });
	}

	Some(Response {
		id,
		data: GitRepo::new(&mut records),
	})
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
		.chain(&[if read_git_index { 0 } else { 1 }])
		.chain(&[RECORD_SEPARATOR])
		.copied()
		.collect();

	(hash, res)
}

fn server(
	host: net::SocketAddr,
	args: &mut impl Iterator<Item = impl AsRef<OsStr>>,
) -> io::Result<()> {
	let in_queue = Arc::new(Mutex::new(HashMap::<u64, net::SocketAddr>::new()));
	let in_queue_clone = in_queue.clone();

	let process_name = args.next().unwrap();
	let mut process = Command::new(process_name)
		.args(args)
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::inherit())
		.spawn()?;

	let mut stdin = process.stdin.take().unwrap();
	let mut stdout = BufReader::new(process.stdout.take().unwrap());

	let socket = Arc::new(net::UdpSocket::bind(host)?);
	let socket_clone = socket.clone();

	thread::spawn(move || {
		let socket = socket_clone;
		let in_queue = in_queue_clone;
		let mut buf = Vec::new();

		while let Ok(len) = stdout.read_until(RECORD_SEPARATOR, &mut buf) {
			buf.clear();

			let a = parse_response(&buf[..len]);
			println!("{a:?}");

			let id = from_utf8(a.id)
				.ok()
				.and_then(|s| u64::from_str_radix(s, 16).ok());

			if let Some(id) = id {
				let mut in_queue = in_queue.lock().unwrap();

				if let Some(origin) = in_queue.remove(&id) {
					match socket.send_to(&a[1], origin) {
						Ok(_) => {}
						Err(e) => {
							eprintln!("Sending error: {origin} -> {e:?}");
						}
					}
				}
			}
		}
	});

	let mut buf = [0; 4096];

	loop {
		let (bytes, origin) = socket.recv_from(&mut buf)?;
		if origin.ip() != host.ip() {
			continue;
		}

		let (hash, request) =
			make_request::<DefaultHasher>(&buf[..bytes], true);

		stdin.write(&request)?;
		stdin.flush()?;

		{
			in_queue.lock().unwrap().insert(hash, origin);
		}
	}
}

fn client(from: net::SocketAddr, host: net::SocketAddr) -> io::Result<()> {
	let socket = net::UdpSocket::bind(from)?;
	socket.connect(host)?;

	let mut buf = [0; 4096];

	socket.send("hi there".as_bytes())?;
	let bytes = socket.recv(&mut buf)?;
	println!("{:?}", &buf[0..bytes]);
	Ok(())
}

fn main() -> io::Result<()> {
	let mut args = env::args_os().skip(1);
	println!("{args:?}");

	let host = net::SocketAddr::from_str("[::1]:8080").unwrap();
	let from = net::SocketAddr::from_str("[::1]:8081").unwrap();

	let subcommand = args.next().expect("Usage: cmd <subcommand>");
	let subcommand = subcommand.to_string_lossy();
	if subcommand == "server" {
		server(host, &mut args)
	} else if subcommand == "client" {
		client(from, host)
	} else {
		eprintln!("Unknown subcommand: {subcommand}");
		Ok(())
	}
}
