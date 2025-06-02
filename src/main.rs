use std::net::SocketAddr;
use std::sync::Arc;
use std::{fs, io};
use std::path::PathBuf;

use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::ServerConfig;
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

use hyper::server::conn::http1;
use hyper::{Request, Response};

use clap::Parser;
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
	/// What folder to serve from
	#[arg()]
	basefolder: String,

	/// Address to serve on
	#[arg()]
	address: SocketAddr,

	/// Whether or not to use http.  By default uses https.
	#[arg(short='h', long)]
	use_http: bool
}

async fn serve(req: Request<Incoming>, path: PathBuf)
			   -> Result<Response<Full<Bytes>>, http::Error> {
	unimplemented!()
	// need to:
	// - make sure requests can't get out of the basedir (making it absolute and no '..' should do)
	// - find request targets, and handle them based on type
	//
}

#[tokio::main]
async fn main() {
	let args = match Args::try_parse() {
		Ok(a) => a,
		Err(e) => {
			let _ = e.print();
			return
		}
	};

	let addr = args.address;
	let Ok(listener) = TcpListener::bind(&addr).await else {
		println!("Could not bind to provided address");
		return
	};

	if let Err(e) = if args.use_http {
		http_server(listener, args.basefolder).await
	} else {
		https_server(listener, args.basefolder).await
	} {
		println!("{}", e);
	};
}

fn error(err: String) -> io::Error {
	io::Error::new(io::ErrorKind::Other, err)
}

async fn https_server(listener: TcpListener, path: String) -> Result<
		(),
		Box<dyn std::error::Error + Send + Sync>
	> {
	// Set a process wide default crypto provider.
	let _ = rustls::crypto::ring::default_provider().install_default();

	// Load public certificate.
	let certs = load_certs("examples/sample.pem")?;
	// Load private key.
	let key = load_private_key("examples/sample.rsa")?;

	// Build TLS configuration.
	let mut server_config = ServerConfig::builder()
		.with_no_client_auth()
		.with_single_cert(certs, key)
		.map_err(|e| error(e.to_string()))?;
	server_config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec(), b"http/1.0".to_vec()];
	let tls_acceptor = TlsAcceptor::from(Arc::new(server_config));

	let basedir = PathBuf::from(path);
	if !basedir.is_dir() {
		return Err("Base directory is not a directory!".into())
	}


	loop {
		let basedir = basedir.clone();
		let (tcp_stream, _) = listener.accept().await?;
		let tls_acceptor = tls_acceptor.clone();
		tokio::spawn(async move {
			let tls_stream = match tls_acceptor.accept(tcp_stream).await {
				Ok(tls_stream) => tls_stream,
				Err(err) => {
					eprintln!("failed to perform tls handshake: {err:#}");
					return;
				}
			};
			if let Err(err) = http1::Builder::new()
				.serve_connection(
					TokioIo::new(tls_stream),
					service_fn(|req|
						serve(req, basedir.clone())
					)
				).await
			{
				eprintln!("failed to serve connection: {err:#}");
			};
		});
	}
}

// Load public certificate from file.
fn load_certs(filename: &str) -> io::Result<Vec<CertificateDer<'static>>> {
	// Open certificate file.
	let certfile = fs::File::open(filename)
		.map_err(|e| error(format!("failed to open {}: {}", filename, e)))?;
	let mut reader = io::BufReader::new(certfile);

	// Load and return certificate.
	rustls_pemfile::certs(&mut reader).collect()
}

// Load private key from file.
fn load_private_key(filename: &str) -> io::Result<PrivateKeyDer<'static>> {
	// Open keyfile.
	let keyfile = fs::File::open(filename)
		.map_err(|e| error(format!("failed to open {}: {}", filename, e)))?;
	let mut reader = io::BufReader::new(keyfile);

	// Load and return a single private key.
	rustls_pemfile::private_key(&mut reader).map(|key| key.unwrap())
}

async fn http_server(listener: TcpListener, path: String) -> Result<
		(),
		Box<dyn std::error::Error + Send + Sync>
		> {
	let basedir = PathBuf::from(path);
	if !basedir.is_dir() {
		return Err("Base directory is not a directory!".into())
	}
	loop {
		let (tcp_stream, _) = listener.accept().await?;
		let basedir = basedir.clone();
		// Use an adapter to access something implementing `tokio::io` traits as if they implement
		// `hyper::rt` IO traits.

		// Spawn a tokio task to serve multiple connections concurrently
		tokio::task::spawn(async move {
			// Finally, we bind the incoming connection to our `hello` service
			if let Err(err) = http1::Builder::new()
				// `service_fn` converts our function in a `Service`
				.serve_connection(
					TokioIo::new(tcp_stream),
					service_fn(|req| {
						serve(req, basedir.clone())
					})
				).await
			{
				eprintln!("Error serving connection: {:?}", err);
			}
		});
	}
}
