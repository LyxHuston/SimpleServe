use http::{Error, response::Builder};
use http_body_util::{BodyExt, Full};
use hyper::{
	Request, Response,
	body::{Body, Bytes, Incoming},
};
use std::{
	fs::File,
	io::{Read, Seek, Write},
	path::{Path, PathBuf},
	process::{Child, Command, Stdio},
};

use is_executable::IsExecutable;

use cmd_lib::run_fun;

use tempfile::tempfile;

// copied from Midnight Machinations (the game)
// https://github.com/midnight-machinations/midnight-machinations/blob/main/server/src/lib.rs
#[macro_export] macro_rules! log {
    // Each case in this macro definition is for a different log marker.
    // None
    ($expr:expr) => {
        println!("\x1b[0;90m{}\x1b[0m {}", chrono::Local::now().format("%m.%d %I:%M:%S"), $expr)
    };
    // Fatal error
    (fatal $prefix:expr; $($expr:expr),*) => {
        log!(&format!("\x1b[0;1;91m[{}] FATAL\x1b[0m \x1b[0;1;41m{}\x1b[0m", $prefix, &format!($($expr),*)))
    };
    // Warning error
    (error $prefix:expr; $($expr:expr),*) => {
        log!(&format!("\x1b[0;1;91m[{}] WARN\x1b[0m {}", $prefix, &format!($($expr),*)))
    };
    // Important
    (important $prefix:expr; $($expr:expr),*) => {
        log!(&format!("\x1b[0;1;93m[{}]\x1b[0m {}", $prefix, &format!($($expr),*)))
    };
    // Info
    (info $prefix:expr; $($expr:expr),*) => {
        log!(&format!("\x1b[0;1;32m[{}]\x1b[0m {}", $prefix, &format!($($expr),*)))
    };
    // Default (use info)
    ($prefix:expr; $($expr:expr),*) => {
        log!(info $prefix; $($expr),*)
    };
}

#[derive(Debug)]
struct OriginWrap<T> {
	data: T,
	origin: PathBuf,
}

#[derive(Debug)]
struct HasStatus<T> {
	data: T,
	status: u16,
}

type BackTrackState = Result<ProcessingState, ProcessingState>;
#[inline(always)]
#[allow(non_snake_case)]
fn Done(p: ProcessingState) -> BackTrackState {
	Err(p)
}
#[inline(always)]
#[allow(non_snake_case)]
fn BackTrack(p: ProcessingState) -> BackTrackState {
	Ok(p)
}

#[inline(always)]
fn inner(s: BackTrackState) -> ProcessingState {
	match s {
		Ok(p) => p,
		Err(p) => p,
	}
}

#[derive(Debug)]
enum ProcessingState {
	ErrorCode(u16),
	InternalError(u16, String),
	Static(HasStatus<OriginWrap<File>>),
	Chain(HasStatus<Vec<OriginWrap<Child>>>),
	HttpError(Error)
}

use ProcessingState::*;

fn status_is_ok(status: u16) -> bool {
	(200..300).contains(&status)
}

impl ProcessingState {
	//#[inline(always)]
	fn halt_processing(&mut self) {
		let Chain(proc) = self else { return };
		for child in &mut proc.data {
			let _ = child.data.kill();
		}
	}

	fn status(&self) -> u16 {
		match self {
			ErrorCode(e) => *e,
			InternalError(e, _) => *e,
			Static(HasStatus { data: _, status: e }) => *e,
			Chain(HasStatus { data: _, status: e }) => *e,
			HttpError(_) => 500,
		}
	}

	fn handle_code(&self) -> Option<u16> {
		match self {
			ErrorCode(e) => Some(*e),
			InternalError(e, m) => {
				log!(error "ERROR"; "{}", m);
				Some(*e)
			},
			Static(HasStatus{
				data:_,
				status
			}) => Some(*status),
			// this method is used to decide what to do with a static file.
			// need to decide how to handle the chain.  Want to at least
			// completely resolve it.
			Chain(_) => {
				log!(fatal "FATAL"; "Attempting to direct a chain into a static file.  This is unimplemented, erroring.");
				todo!()
			},
			_ => None
		}
	}

	fn is_ok(&self) -> bool {
		status_is_ok(self.status())
	}
}

/// due to a restriction of unix, exit codes of a program are only a u8,
/// which is not large enough for all HTTP status codes.  So, here's a
/// list of all exit codes known by this program.  Return one with
/// `$<exit code>` instead of hard-coding it
/// list from https://en.wikipedia.org/wiki/List_of_HTTP_status_codes
/// to add more acceptable status codes, extend this list
pub const EXIT_CODES: &[u16] = &[
	// special case: successful execution should return a success
	200,
	// 1**, informational response
	100, 101, 102, 103,
	// 2**, success
	200, 201, 202, 203, 204, 205, 206, 207, 208, 226,
	// 3**, redirection
	300, 301, 302, 303, 304, 305, 306, 307, 308,
	// 4**, client errors
	400, 401, 402, 403, 404, 405, 406, 407, 408, 409, 410, 411, 412, 413, 414, 415, 416, 417, 418,
	421, 422, 423, 424, 425, 426, 428, 429, 431, 451,
	// 5**, server errors
	500, 501, 502, 503, 504, 505, 506, 507, 508, 510, 511,
];

#[inline(always)]
fn to_exit_code(res: Option<i32>) -> u16 {
	res.map(|r| *(EXIT_CODES.get(r as usize).unwrap_or(&500u16)))
		.unwrap_or(500u16)
}

// args passed to commands are:
// uri_path, METHOD "" headers "" url parameters "" path parameters (server does not get fragment)
fn handle_file(
	file: &Path,
	mut prev_state: ProcessingState,
	params: &Vec<String>
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
		if prev_state.is_ok() {
			prev_state.halt_processing();
			ErrorCode(404)
		} else {
			// if it is not ok, this should be done during passthrough for error checking.
			// so, no need to halt processing.
			prev_state
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
				),
			);
		};
		let Ok(headers) = tempfile() else {
			prev_state.halt_processing();
			return InternalError(500, String::from("Could not create header tempfile"));
		};
		let (input_opt, mut prev_chain, status) = match prev_state {
			Chain(mut v) => (
				v.data
					.last_mut()
					.map(|c| c.data.stdout.take())
					.unwrap_or(None)
					.map(Stdio::from),
				v.data,
				v.status,
			),
			Static(b) => (Some(Stdio::from(b.data.data)), Vec::new(), b.status),
			a => (tempfile().ok().map(Stdio::from), Vec::new(), a.status()),
		};
		let Some(input) = input_opt else {
			for mut c in prev_chain {
				let _ = c.data.kill();
			}
			return InternalError(
				500,
				"Could not ascertain input from previous processing state".to_string(),
			);
		};
		let Ok(child) = Command::new(&file)
			.current_dir(work_dir)
			.args(params)
			.stdin(input)
			.stderr(headers)
			.stdout(Stdio::piped())
			.spawn()
		else {
			for mut c in prev_chain {
				let _ = c.data.kill();
			}
			return InternalError(
				500,
				format!("Error running command {}", file.to_string_lossy()),
			);
		};
		prev_chain.push(OriginWrap {
			data: child,
			origin: file,
		});
		Chain(HasStatus {
			data: prev_chain,
			status,
		})
	} else {
		// if exists, not executable, not a folder, return whatever original status,
		// Content-type mime-type, and the file

		// process chains currently panic with a todo here.
		let Some(c) = prev_state.handle_code() else {
			return InternalError(
				500,
				format!("Non-error code handed to static file {}", file.to_string_lossy()),
			);
		};
		let Ok(open_file) = File::open(&file) else {
			return InternalError(
				500,
				format!("Couldn't open file {}", file.to_string_lossy()),
			);
		};
		Static(HasStatus {
			data: OriginWrap {
				data: open_file,
				origin: file,
			},
			status: c,
		})
	}
}

fn handle_layer(
	curr_layer: &mut PathBuf,
	remaining_layers: &[String],
	params: &mut Vec<String>,
	incoming_body: ProcessingState,
) -> BackTrackState {
	let res = if remaining_layers.is_empty() {
		handle_file(curr_layer, incoming_body, params)
	} else if remaining_layers[0].starts_with(".") {
		// hide hidden files/directories and prevent escape through '..'
		ErrorCode(403)
	} else {
		curr_layer.push(remaining_layers[0].clone());
		let res = handle_layer(curr_layer, &remaining_layers[1..], params, incoming_body)?;
		curr_layer.pop();
		res
	};
	// if there is a base file, stop the backtracking and post-processing
	curr_layer.push(".base");
	if curr_layer.exists() {
		return Done(res);
	}
	curr_layer.pop();
	BackTrack(res)
}

fn error_response (e: u16) -> Result<Response<Full<Bytes>>, Error> {
	let message = format!(
		"Error {}: That's all we know",
		e
	);
	Builder::new()
		.status(e)
		.header("Content-Type", "text/plain; charset=us-ascii")
		.header("Content-Length", message.len())
		.body(Full::new(Bytes::from(message)))
}

fn resolve_to_response_inner(
	status: ProcessingState,
	basepath: &PathBuf,
	params: &Vec<String>,
	layers: &[String]
) -> Result<Result<Response<Full<Bytes>>, Error>, ProcessingState> {
	match status {
		ErrorCode(e) => Ok(error_response(e)),
		InternalError(e, msg) => {
			log!(error "ERROR"; "{}", msg);
			Ok(error_response(e))
		}
		Static(HasStatus {
			data: OriginWrap {
				data: mut f,
				origin: p,
			},
			status,
		}) => {
			let mut data = Vec::new();
			f.rewind().map_err(|e| {
				InternalError(
					500,
					format!("Unable to rewind to start of file while resolving to response: {}", e),
				)
			})?;
			f.read_to_end(&mut data)
			 .map_err(|e| InternalError(500, format!("Couldn't read file {}: {}", p.display(), e)))?;
			let mimetype = run_fun!(file -ib $p).map_err(|e| {
				InternalError(500, format!("Error getting mimetype of {}: {}", p.display(), e))
			})?;
			Ok(Builder::new()
				.status(status)
				.header("Content-Type", mimetype)
				.header("Content-Length", data.len())
				.body(Full::new(Bytes::from(data))))
		}
		HttpError(e) => Ok(Err(e)),
		Chain(HasStatus { data: mut c, status }) => {
			let mut error: Option<(PathBuf, u16)> = None;
			for OriginWrap {
				data: child,
				origin,
			} in c.iter_mut()
			{
				if error.is_none() {
					let status_data = child.wait().map_err(|e| {
						InternalError(
							500,
							format!("Error resolving process chain at {}: {}", origin.display(), e),
						)
					})?;
					let code = to_exit_code(status_data.code());
					if status_is_ok(code) {
						continue;
					}
					error = Some((origin.clone(), code))
				} else {
					let _ = child.kill();
				}
			}
			if let Some((origin, code)) = error {
				let len = origin.components().count()
					.saturating_sub(basepath.components().count())
					.saturating_sub(1);
				let mut p = params.clone();
				let mut b = basepath.clone();
				resolve_to_response_inner(
					inner(handle_layer(
						&mut b,
						// only situation min statement should be useful is when something came from an
						// index or error file. 
						&layers[..len.clamp(0, layers.len())],
						&mut p,
						ErrorCode(code)
					)),
					basepath,
					params,
					layers
				)
			} else {
				let last = c
					.pop()
					.ok_or(InternalError(500, "Resolving empty chain".to_string()))?;
				let output = last
					.data
					.wait_with_output()
					.map_err(
						|e| InternalError(500, format!("End of chain could not capture output: {}", e))
					)?;
				Ok(String::from_utf8(output.stderr)
					.map_err(
						|e| InternalError(
							500,
							format!("Error reading utf-8 from header output: {}", e)
						)
					)?
					.split("\n")
					.filter_map(|s| s.split_once("="))
					.fold(
						Builder::new()
							.status(status),
						|b, (k, v)| b.header(k, v)
					)
					.header("Content-Length", output.stdout.len())
					.body(output.stdout.into()))
			}
		}
	}
}

fn resolve_to_response(
	status: ProcessingState,
	basepath: PathBuf,
	params: &Vec<String>,
	layers: &[String]
) -> Result<Response<Full<Bytes>>, Error> {
	match resolve_to_response_inner(status, &basepath, params, layers) {
		Ok(o) => o,
		Err(e) => resolve_to_response(e, basepath, params, layers),
	}
}

/// uri_path, METHOD "" headers "" url parameters "" path parameters (server does not get fragment)
/// (parameters, layers)
fn get_params_and_layers(parts: http::request::Parts) -> (Vec<String>, Vec<String>) {
	(
		[
			String::from(parts.uri.path()),
			parts.method.to_string(),
			"".to_string()
		]
			.into_iter()
			.chain(
				parts
					.headers
					.into_iter()
					.filter_map(
						|(name_opt, val)|
						val
							.to_str()
							.ok()
							.map(
								|val|
								format!(
									"{}{}",
									name_opt
										.map_or(
											"".to_string(),
											|name| format!("{}=", name)
										),
									val
								)
							)
					)
					.filter(|s| !s.is_empty())
			)
			.chain(["".to_string()])
			.chain(
				parts
					.uri
					.query()
					.unwrap_or("")
					.split("&")
					.filter(|s| !s.is_empty())
					.map(String::from)
			)
			.chain(["".to_string()])
			.collect::<Vec<String>>(),
		parts.uri.path().split("/").filter(|p| !p.is_empty()).map(String::from).collect::<Vec<String>>()
	 )
}

async fn serve_help(body: Incoming, path: PathBuf, params: &[String], layers: &[String]) -> ProcessingState {
	// get the path
	let mut path = path.clone();

	let mut params = Vec::from(params);
	
	// open tempfile for input data and put it in
	let Ok(mut inp) = tempfile() else {
		return InternalError(500, "Unable to create tempfile for buffer.".to_string());
	};
	let Ok(body_data) = body.collect().await else {
		return InternalError(500, "Unable to collect entire incoming body.".to_string());
	};
	let bytes = body_data.to_bytes();
	let Ok(_) = inp.write_all(&bytes) else {
		return InternalError(
			500,
			"Unable to write incoming body to temp file.".to_string(),
		);
	};
	let Ok(_) = inp.flush() else {
		return InternalError(500, "Unable to flush temp file.".to_string());
	};
	let Ok(_) = inp.rewind() else {
		return InternalError(500, "Unable to rewind temp file.".to_string());
	};
	// handle it, then go over the output
	inner(handle_layer(
		&mut path,
		layers,
		&mut params,
		Static(HasStatus {
			data: OriginWrap {
				data: inp,
				origin: "incoming".into(),
			},
			status: 200,
		}),
	))
}

pub async fn serve(req: Request<Incoming>, path: PathBuf) -> Result<Response<Full<Bytes>>, Error> {
	let (parts, body) = req.into_parts();
	let (params, layers) = get_params_and_layers(parts);
	let mut resp = resolve_to_response(
		serve_help(body, path.clone(), &params, &layers).await,
		path,
		&params,
		&layers
	)?;
	if let Some(size) = resp.size_hint().exact() {
		resp.headers_mut().insert("Content-Length", size.into());
	}
	Ok(resp)
}
