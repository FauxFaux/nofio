use failure::Error;

fn main() -> Result<(), Error> {
    pretty_env_logger::init();

    let mut net = nofio::Net::empty()?;
    net.tcp_listen(&"127.0.0.1:6060".parse()?)?;
    loop {
        let ev = net.next()?;
        match ev {
            nofio::Event::Data(token) => {
                let mut io = net.io(token);
                let buf = io.buf();
                if let Some(header_end) = find_subsequence(buf, b"\r\n\r\n") {
                    println!(
                        "headers: {:?}",
                        String::from_utf8(buf[..header_end].to_vec())
                    );
                    io.consume(header_end);

                    io.write(b"HTTP/1.0 200 OK\r\n\r\n");
                    io.close();
                }
            }
            nofio::Event::Done(token, _) => net.io(token).close(),
            _ => println!("{:?}", ev),
        }
    }
    Ok(())
}

// https://stackoverflow.com/questions/35901547/how-can-i-find-a-subsequence-in-a-u8-slice
fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|pos| pos + needle.len())
}
