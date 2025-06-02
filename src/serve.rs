use std::{path::PathBuf, process::Command, fs::File, io::{Read, Write}};
use hyper::{
	Request, Response,
	body::{Bytes, Incoming}
};
use http::{Error, response::Builder};
use http_body_util::{Full, BodyExt};

use is_executable::IsExecutable;

use cmd_lib::run_fun;

use tempfile::tempfile;

enum ResultType {
	ErrorCode(u16),
	InternalError(u16, String),
	Body(Response<File>),
	HttpError(Error)
}


/// due to a restriction of unix, exit codes of a program are only a u8,
/// which is not large enough for all HTTP status codes.  So, here's a
/// list of all exit codes known by this program.  Return one with
/// `$<exit code>` instead of hard-coding it
/// list from https://en.wikipedia.org/wiki/List_of_HTTP_status_codes
/// to add more acceptable status codes, extend this list
pub const EXIT_CODES : &[u16] = &[
	// special case: successful execution should return a success
	200,
	// 1**, informational response
	100, 101, 102, 103,
	// 2**, success
	200, 201, 202, 203, 204, 205, 206, 207, 208, 226,
	// 3**, redirection
	300, 301, 302, 303, 304, 305, 306, 307, 308,
	// 4**, client errors
	400, 401, 402, 403, 404, 405, 406, 407, 408, 409, 410, 411, 412,
	413, 414, 415, 416, 417, 418, 421, 422, 423, 424, 425, 426, 428,
	429, 431, 451,
	// 5**, server errors
	500, 501, 502, 503, 504, 505, 506, 507, 508, 510, 511
];


#[inline(always)]
fn to_exit_code(res: Option<i32>) -> u16 {
	res.map(|r| *(EXIT_CODES.get(r as usize).unwrap_or(&500u16))).unwrap_or(500u16)
}


// args passed to commands are uri_path, METHOD and then mappings (server does not get fragment)
fn handle_file(file: &PathBuf, stdin: File, params: &Vec<String>) -> ResultType {
	// there are many time-of-check time-of-use race conditions here.
	// this is fine, because it's not expecting to be serving from
	// directories that are changing frequently
	if !file.exists() {
		ResultType::ErrorCode(404)
	} else if file.is_dir() {
		// I am a teapot: I am a dir
		ResultType::ErrorCode(418)
	} else if file.is_executable() {
		let Some(work_dir) = file.parent() else {
			// if it cannot determine the parent, that means it's already at root.  Which is bad.
			// and not just because this shouldn't be running on a dir
			return ResultType::InternalError(
				500,
				format!(
					"Could not determine parent folder of {}",
					file.to_string_lossy()
				)
			);
		};
		let Ok(headers)	= tempfile() else {return ResultType::InternalError(
			500, String::from("Could not create header tempfile"))
		};
		let Ok(mut output)	= tempfile() else {return ResultType::InternalError(
			500, String::from("Could not create output tempfile"))
		};
		// TODO: use piping and child processes instead of blocking?
		// because of how Command stdout works, might actually be easier to do so.
		let Ok(mut child) = Command::new(file)
			.current_dir(work_dir)
			.args(params)
			.stdin(stdin)
			.stderr(headers)
			.stdout(output)
			.spawn() else {return ResultType::InternalError(
				500, format!("Error running command {}", file.to_string_lossy()))
			};
		let full_output = child.wait_with_output();
		let Some(mut header_out) = child.stderr else {unreachable!()};
		let Some(mut data_out) = child.stdout else {unreachable!()};
		let code = to_exit_code(Some(1));
		if 200 <= code && code < 300 {
			let mut header_string = String::new();
			let Ok(_) = header_out.read_to_string(&mut header_string) else {
				return ResultType::InternalError(
					500, String::from("Error reading from header temporary file!")
				)
			};
			header_string.split("\n").filter_map(|s| s.split_once("=")).fold(
				Builder::new().status(code),
				|b, (k, v)| {
					b.header(k, v)
				}
			).body(child.wa).map_or_else(
				|err|	ResultType::HttpError(err),
				|ok|	ResultType::Body(ok)
			)
		} else {
			ResultType::ErrorCode(code)
		}
	} else {
		// if exists, not executable, not a folder, return 200, Content-type mime-type, and the file 
		let Ok(open_file) = File::open(file) else {
			// couldn't open file but it does exist
			return ResultType::InternalError(
				500,
				format!("Couldn't open file {}", file.to_string_lossy())
			);
		};
		let Ok(mimetype) = run_fun!(file -ib $file) else {
			// couldn't get mimetype
			return ResultType::InternalError(
				500,
				format!("Error getting mimetype of {}", file.to_string_lossy())
			);
		};
		Builder::new()
			.status(200)
			.header("Content-Type", mimetype)
			.body(open_file)
			.map_or_else(
				|err|	ResultType::HttpError(err),
				|ok|	ResultType::Body(ok)
			)
	}
}


fn handle_layer(
	curr_layer: &mut PathBuf, remaining_layers: &[String],
	params: &mut Vec<String>, incoming_body: &File
) -> ResultType {
	if remaining_layers.is_empty() {
		if curr_layer.is_dir() {
			curr_layer.push(".index");
		}
	} else if remaining_layers[0].starts_with(".") {
		// hide hidden files/directories and prevent escape through '..'
		return ResultType::ErrorCode(403);
	} else {
		curr_layer.push(remaining_layers[0].clone());
		handle_layer(curr_layer, &remaining_layers[1..], params, incoming_body);
		curr_layer.pop();
	}
	return ResultType::ErrorCode(404);
}

pub async fn serve(req: Request<Incoming>, path: PathBuf)
			   -> Result<Response<Full<Bytes>>, Error> {
	// get the path
	let mut path = path.clone();
	// split into parts
	let (parts, body) = req.into_parts();
	// process the parameters
	let mut params = [
		String::from(parts.uri.path()),
		parts.method.to_string()
	].into_iter().chain(
		parts.uri.query().unwrap_or("").split("&").map(String::from)
	).collect::<Vec<String>>();
	// split uri path request into layers to be iterated over
	let layers = parts.uri.path().split("/").map(String::from).collect::<Vec<String>>();
	// open tempfile for input data and put it in
	let Ok(mut inp) = tempfile() else {
		println!("Unable to create tempfile for buffer.");
		return Builder::new().status(500).body(
			Full::new(Bytes::from(format!("Error 500: That's all we know")))
		)
	};
	let Ok(body_data) = body.collect().await else {
		println!("Unable to collect entire incoming body.");
		return Builder::new().status(500).body(
			Full::new(Bytes::from(format!("Error 500: That's all we know")))
		)
	};
	inp.write_all(&*body_data.to_bytes());
	// handle it, then go over the output
	match handle_layer(&mut path, &layers[..], &mut params, &inp) {
		ResultType::ErrorCode(e) =>
			Builder::new().status(e).body(
				Full::new(Bytes::from(format!("Error {}: That's all we know", e)))
			),
		ResultType::InternalError(e, msg) => {
			println!("{}", msg);
			Builder::new().status(e).body(
				Full::new(Bytes::from(format!("Error {}: That's all we know", e)))
			)
		}
		ResultType::Body(b) => {
			let mut bytes: Vec<u8> = Vec::new();
			let Ok(size) = b.body().read_to_end(&mut bytes) else {
				println!("Error reading back from file");
				return Builder::new().status(500).body(
					Full::new(Bytes::from(format!("Error 500: That's all we know")))
				)
			};
			b.headers().iter().fold(
				Builder::new().status(b.status()),
				|build, (key, val)| build.header(key, val)
			)
			.header("Content-Length", size)
			.body(Full::new(Bytes::from(bytes)))
		},
		ResultType::HttpError(e) => Err(e)
	}
	// need to:
	// - find request targets, and handle them based on type
	// - errors handling
}
