use std::{
    error::Error,
    net::SocketAddr,
    path::Path,
    pin::Pin,
    process,
    str::FromStr,
    time::Duration
};
use chrono::Local;
use colored::Colorize;
use env_logger::Builder;
use log::{error, info, trace, Level};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::timeout;
use std::io::Write;
use utp::udp_stream::{UdpListener, UdpStream};

const UDP_BUFFER_SIZE: usize = 17480; // 17kb
const UDP_TIMEOUT: u64 = 10 * 1000; // 10sec

fn usage() -> ! {
    println!("Usage: utp [-s|-c] <address> <port>");
    process::exit(1);
}

fn init_logger() {
    Builder::new()
        .format(|buf, record| {
            let file = record.file().unwrap_or("unknown");
            let line = record.line().unwrap_or(0);
            let base_dir = env!("CARGO_MANIFEST_DIR");
            let file_path = Path::new(file);
            let base_path = Path::new(base_dir);
            let relative_path = file_path
                .strip_prefix(base_path)
                .map(|path| path.to_str().unwrap_or(file))
                .unwrap_or(file);

            let timestamp = Local::now()
                .format("%Y-%m-%dT%H:%M:%S")
                .to_string()
                .yellow();

            let level_colored = match record.level() {
                Level::Error => "ERROR".bright_red().bold(),
                Level::Warn => "WARN".yellow().bold(),
                Level::Info => "INFO".green().bold(),
                Level::Debug => "DEBUG".cyan(),
                Level::Trace => "TRACE".bright_white(),
            };

            let file_colored = relative_path.blue();
            let location = format!("{}:{}", file_colored, line);
            let message = format!("{}", record.args()).white().bold();

            writeln!(
                buf,
                "{} [{}] [{}] - {}",
                timestamp,
                level_colored,
                location,
                message
            )
        })
        .filter_level(log::LevelFilter::Info)
        .init();
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    init_logger();

    let mut args = std::env::args();
    args.next();

    enum Mode {
        Server,
        Client,
    }

    let mode = match args.next() {
        Some(ref s) if s == "-s" => Mode::Server,
        Some(ref s) if s == "-c" => Mode::Client,
        _ => usage(),
    };

    let addr = match (args.next(), args.next()) {
        (None, None) => "127.0.0.1:8080".to_owned(),
        (Some(ip), Some(port)) => format!("{}:{}", ip, port),
        _ => usage(),
    };

    let addr = SocketAddr::from_str(&addr)?;

    match mode {
        Mode::Server => {
            let listener = UdpListener::bind(addr).await?;
            info!("Server listening on {}", addr);
            loop {
                let (mut stream, _) = listener.accept().await?;
                tokio::spawn(async move {
                    let id = std::thread::current().id();
                    let block = async move {
                        let mut buf = vec![0u8; UDP_BUFFER_SIZE];
                        let duration = Duration::from_millis(UDP_TIMEOUT);
                        loop {
                            match timeout(duration, stream.read(&mut buf)).await {
                                Ok(Ok(n)) if n > 0 => {
                                    stream.write_all(&buf[..n]).await?;
                                    trace!("Thread {:?} echoed {:?} for {} bytes", id, stream.peer_addr(), n);
                                }
                                Ok(Err(e)) => {
                                    error!("Read error: {:?}", e);
                                    break;
                                }
                                Err(_) => {
                                    error!("Timeout reached for {:?}", stream.peer_addr());
                                    continue; // Continue waiting for the next data.
                                }
                                _ => break,
                            }
                        }
                        Ok::<(), std::io::Error>(())
                    };
                    if let Err(e) = block.await {
                        error!("Error: {:?}", e);
                    }
                });
            }
        }

        Mode::Client => {
            let mut client = UdpStream::connect(addr).await?;
            info!("Connected to server at {}", addr);
            let mut buffer = String::new();
            loop {
                std::io::stdin().read_line(&mut buffer)?;
                client.write_all(buffer.as_bytes()).await?;
                let mut buf = vec![0u8; 1024];
                let n = client.read(&mut buf).await?;
                info!("-> {}", String::from_utf8_lossy(&buf[..n]));
                buffer.clear();
            }
        }
    }
}
