use std::{fs::File, io::{Read, Seek, Write}, path::{Path, PathBuf}, process::{Child, Command, Stdio}};
use hyper::{
	Request, Response,
	body::{Bytes, Incoming}
};
use http::{Error, response::Builder};
use http_body_util::{Full, BodyExt};

use is_executable::IsExecutable;

use cmd_lib::run_fun;

use tempfile::tempfile;

struct OriginWrap<T> {
	data: T,
	origin: PathBuf
}

struct HasStatus<T> {
	data: T,
	status: u16
}

type BackTrackState = Result<ProcessingState, ProcessingState>;
#[inline(always)]
#[allow(non_snake_case)]
fn Done(p: ProcessingState) -> BackTrackState {Err(p)}
#[inline(always)]
#[allow(non_snake_case)]
fn BackTrack(p: ProcessingState) -> BackTrackState {Ok(p)}

#[inline(always)]
fn inner(s: BackTrackState) -> ProcessingState {
	match s {
		Ok(p) => p,
		Err(p) => p
	}
}


enum ProcessingState {
	ErrorCode(u16),
	InternalError(u16, String),
	Static(HasStatus<OriginWrap<File>>),
	Chain(HasStatus<Vec<OriginWrap<Child>>>),
	HttpError(Error)
}

use ProcessingState::*;

impl ProcessingState {
	//#[inline(always)]
	fn halt_processing(&mut self) {
		let Chain(proc) = self else {return};
		for child in &mut proc.data {
			let _ = child.data.kill();
			let _ = child.data.wait();
		}
	}

	fn status(&self) -> u16 {
		match self {
			ErrorCode(e) => *e,
			InternalError(e, _) => *e,
			Static(HasStatus{data:_, status:e}) => *e,
			Chain(HasStatus{data:_, status:e}) => *e,
			HttpError(_) => 500
		}
	}
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
fn handle_file(
	file: &Path, mut prev_state: ProcessingState, params: &Vec<String>, pass_if_missing: bool
) -> ProcessingState {
	// there are many time-of-check time-of-use race conditions here.
	// this is fine, because it's not expecting to be serving from
	// directories that are changing frequently
	if let HttpError(e) = prev_state {
		return HttpError(e); // just forward it.  Don't know and isn't my responsibility to handle these
	}
	let mut file = file.to_path_buf();
	if file.is_dir() {
		file.push(".index")
	}
	if !file.exists() {
		if pass_if_missing {
			prev_state
		} else {
			prev_state.halt_processing();
			ErrorCode(404)
		}
	} else if file.is_dir() {
		// I am a teapot: I am a dir
		prev_state.halt_processing();
		ErrorCode(418)
	} else if file.is_executable() {
		let Some(work_dir) = file.parent() else {
			// if it cannot determine the parent, that means it's already at root.  Which is bad.
			// and not just because this shouldn't be running on a dir
			prev_state.halt_processing();
			return InternalError(
				500,
				format!(
					"Could not determine parent folder of {}",
					file.to_string_lossy()
				)
			);
		};
		let Ok(headers)	= tempfile() else {
			prev_state.halt_processing();
			return InternalError(
				500, String::from("Could not create header tempfile"))
		};
		let (input_opt, mut prev_chain, status) = match prev_state {
			Chain(mut v) => (v.data
				.last_mut()
				.map(|c| c.data.stdout.take())
				.unwrap_or(None)
				.map(Stdio::from), v.data, v.status),
			Static(b) => (Some(Stdio::from(b.data.data)), Vec::new(), b.status),
			a => (tempfile().ok().map(Stdio::from), Vec::new(), a.status())
		};
		let Some(input) = input_opt else {
			for mut c in prev_chain {
				let _ = c.data.kill();
			}
			return InternalError(
				500, "Could not ascertain input from previous processing state".to_string())
		};
		let Ok(child) = Command::new(&file)
			.current_dir(work_dir)
			.args(params)
			.stdin(input)
			.stderr(headers)
			.stdout(Stdio::piped())
			.spawn() else {
				for mut c in prev_chain {
					let _ = c.data.kill();
				}
				return InternalError(
					500, format!("Error running command {}", file.to_string_lossy()))
			};
		prev_chain.push(OriginWrap{
			data:child,
			origin:file
		});
		Chain(HasStatus{
			data:prev_chain,
			status
		})
	} else {
		// if exists, not executable, not a folder, return 200, Content-type mime-type, and the file 
		prev_state.halt_processing();	// should user be *allowed* to funnel a chain process
										// into a static file?
		let Ok(open_file) = File::open(&file) else {
			return InternalError(
				500,
				format!("Couldn't open file {}", file.to_string_lossy())
			);
		};
		Static(HasStatus{
			data:OriginWrap{
				data:open_file,
				origin:file
			},
			status:200
		})
	}
}


fn handle_layer(
	curr_layer: &mut PathBuf, remaining_layers: &[String],
	params: &mut Vec<String>, incoming_body: ProcessingState
) -> BackTrackState {
	if remaining_layers.is_empty() {
		BackTrack(handle_file(curr_layer, incoming_body, params, false))
	} else if remaining_layers[0].starts_with(".") {
		// hide hidden files/directories and prevent escape through '..'
		BackTrack(ErrorCode(403))
	} else {
		curr_layer.push(remaining_layers[0].clone());
		let res = handle_layer(curr_layer, &remaining_layers[1..], params, incoming_body)?;
		curr_layer.pop();
		// if there is a base file, stop the backtracking and post-processing
		curr_layer.push(".base");
		if curr_layer.exists() {
			return Done(res)
		}
		curr_layer.pop();
		BackTrack(res)
	}
}

fn resolve_to_response_inner(status: ProcessingState)
							 -> Result<Result<Response<Full<Bytes>>, Error>, ProcessingState> {
	match status {
		ErrorCode(e) =>
			Ok(Builder::new().status(e).body(
				Full::new(Bytes::from(format!("Error {}: That's all we know", e)))
			)),
		InternalError(e, msg) => {
			println!("{}", msg);
			Ok(Builder::new().status(e).body(
				Full::new(Bytes::from(format!("Error {}: That's all we know", e)))
			))
		}
		Static(b) => {
			let mut data = Vec::new();
			let HasStatus{
				data:OriginWrap{
					data:mut f,
					origin:p
				},
				status
			} = b;
			f.rewind().map_err(|_| InternalError(
				500,
				"Unable to rewind to start of file while resolving to response".to_string()
			))?;
			f.read_to_end(&mut data).map_err(|_| InternalError(
				500,
				format!("Couldn't read file {}", p.display())
			))?;
			let mimetype = run_fun!(file -ib $p).map_err(|_| InternalError(
				500,
				format!("Error getting mimetype of {}", p.display())
			))?;
			Ok(Builder::new()
				.status(status)
				.header("Content-Type", mimetype)
				.header("Content-Length", data.len())
				.body(Full::new(Bytes::from(data))))
		}
		HttpError(e) => Ok(Err(e)),
		Chain(c) =>
			unimplemented!()
	}
}

fn resolve_to_response(status: ProcessingState) -> Result<Response<Full<Bytes>>, Error> {
	match resolve_to_response_inner(status) {
		Ok(o) => o,
		Err(e) => resolve_to_response(e)
	}
}

async fn serve_help(req: Request<Incoming>, path: PathBuf)
					-> ProcessingState {
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
		return InternalError(500, "Unable to create tempfile for buffer.".to_string());
	};
	let Ok(body_data) = body.collect().await else {
		return InternalError(500, "Unable to collect entire incoming body.".to_string())
	};
	let bytes = body_data.to_bytes();
	let Ok(_) = inp.write_all(&bytes) else {
		return InternalError(500, "Unable to write incoming body to temp file.".to_string())
	};
	let Ok(_) = inp.flush() else {
		return InternalError(500, "Unable to flush temp file.".to_string())
	};
	let Ok(_) = inp.rewind() else {
		return InternalError(500, "Unable to flush temp file.".to_string())
	};
	// handle it, then go over the output
	inner(handle_layer(&mut path, &layers[..], &mut params, Static(HasStatus{
		data:OriginWrap{
			data:inp,
			origin:"incoming".into()
		},
		status:200
	})))
}

pub async fn serve(req: Request<Incoming>, path: PathBuf)
					-> Result<Response<Full<Bytes>>, Error> {
	resolve_to_response(serve_help(req, path).await)
}
