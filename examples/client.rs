use futures::AsyncWriteExt;
use std::net::SocketAddr;
use std::str::FromStr;
use utp::stream::UtpStream;

#[tokio::main]
async fn main() {

    let mut args : Vec<String>= std::env::args().collect();

    if args.len() < 2 {

        eprintln!("Usage: client <port>");
        std::process::exit(1);

    }

    let port: &String = &args[1];

    let addr = format!("127.0.0.1:{}", &port);
    let client_addr = SocketAddr::from_str(&addr).unwrap();
    let mut client_stream : UtpStream = UtpStream::bind(Some(client_addr)).await;
    let server_addr = SocketAddr::from_str("127.0.0.1:8080").unwrap();
    match client_stream.connect(server_addr).await{
        Ok(..) => {

            println!("Connected to {}", server_addr);

            let input = std::io::stdin();

            let mut input_string = String::new();

            loop {
                input_string.clear();
                match input.read_line(&mut input_string){
                    Ok(0) => {
                        eprintln!("cannot read anything from the terminal");
                        break;
                    }
                    Ok(len) => {
                        let data = input_string[..len].as_bytes();
                        match client_stream.write_all(data).await{

                            Ok(_) => println!("Wrote {} bytes", len),
                            Err(error) => println!("error: {}", error),

                        }



                    }
                    Err(error) => {
                        println!("error: {}", error);
                        break;
                    },

                }

            }
        }
        Err(error) => {
            println!("Error: {}", error);
        }

    }

}