use std::collections::{HashMap, VecDeque};
use std::io;
use std::io::Read;
use std::io::Write;
use std::net::SocketAddr;

use failure::Error;
use log::info;
use mio::net::TcpListener;
use mio::net::TcpStream;
use mio::Events;
use mio::PollOpt;
use mio::Ready;
use mio::Token;
use mio_extras::channel as mio_chanel;

const BUF_SIZE: usize = 8 * 1024;

pub struct Net {
    last_token: usize,
    tokens: HashMap<Token, Owned>,
    poll: mio::Poll,
    channel: CommandChannel,
    events: VecDeque<Event>,
}

struct Owned {
    token: Token,
    mode: OwnedMode,
}

enum OwnedMode {
    Server(Server),
    Conn(Conn),
}

struct Server {
    inner: TcpListener,
}

struct Conn {
    inner: TcpStream,
    read_buffer: Stream,
    write_buffer: Stream,
    wants_shutdown: bool,
}

struct Stream {
    inner: Vec<u8>,
    wanted: usize,
    seen_eof: bool,
}

enum Command {}

struct CommandChannel {
    recv: mio_chanel::Receiver<Command>,
    send: mio_chanel::Sender<Command>,
}

#[derive(Debug)]
pub enum Event {
    NewConnection(Token),
    Data(Token),
    Done(Token, Direction),
}

#[derive(Debug)]
pub enum Direction {
    Read,
    Write,
}

pub struct Io<'n> {
    inner: &'n mut Net,
    token: Token,
}

impl Default for CommandChannel {
    fn default() -> CommandChannel {
        let (send, recv) = mio_chanel::channel();
        CommandChannel { send, recv }
    }
}

impl Stream {
    fn has_space(&self) -> bool {
        self.inner.len() < self.wanted
    }

    fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

impl Default for Stream {
    fn default() -> Stream {
        Stream {
            inner: Vec::new(),
            wanted: 8 * 1024,
            seen_eof: false,
        }
    }
}

impl<'n> Io<'n> {
    pub fn buf(&self) -> &[u8] {
        match self.inner.tokens.get(&self.token).unwrap().mode {
            OwnedMode::Conn(ref conn) => &conn.read_buffer.inner,
            _ => unimplemented!("read on others"),
        }
    }

    pub fn consume(&mut self, len: usize) {
        match self.inner.tokens.get_mut(&self.token).unwrap().mode {
            OwnedMode::Conn(ref mut conn) => drop(conn.read_buffer.inner.drain(..len)),
            _ => unimplemented!("consume on others"),
        }
    }

    pub fn write(&mut self, data: &[u8]) {
        match self.inner.tokens.get_mut(&self.token).unwrap().mode {
            OwnedMode::Conn(ref mut conn) => conn.write_buffer.inner.extend_from_slice(data),
            _ => unimplemented!("write on others"),
        }
    }

    pub fn close(&mut self) -> () {
        match self.inner.tokens.get_mut(&self.token).unwrap().mode {
            OwnedMode::Conn(ref mut conn) => {
                conn.wants_shutdown = true;
            }
            _ => unimplemented!("close on others"),
        }
    }
}

const COMMANDS_TOKEN: Token = Token(0);

impl Net {
    pub fn empty() -> Result<Net, Error> {
        let poll = mio::Poll::new()?;
        let channel = CommandChannel::default();
        poll.register(
            &channel.recv,
            COMMANDS_TOKEN,
            Ready::readable(),
            PollOpt::edge(),
        )?;
        Ok(Net {
            last_token: 1,
            poll,
            tokens: Default::default(),
            channel,
            events: VecDeque::new(),
        })
    }

    fn bump_token(&mut self) -> Token {
        self.last_token = self.last_token.checked_add(1).expect("out of tokens!");
        Token(self.last_token)
    }

    pub fn tcp_listen(&mut self, addr: &SocketAddr) -> Result<(), Error> {
        let inner = TcpListener::bind(addr)?;
        let token = self.bump_token();
        self.poll
            .register(&inner, token, Ready::readable(), PollOpt::edge())?;
        self.tokens.insert(
            token,
            Owned {
                token,
                mode: OwnedMode::Server(Server { inner }),
            },
        );
        Ok(())
    }

    pub fn next(&mut self) -> Result<Event, Error> {
        while self.events.is_empty() {
            self.fill()?;
        }

        Ok(self.events.pop_front().expect("non-empty"))
    }

    pub fn io(&mut self, token: Token) -> Io {
        Io { inner: self, token }
    }

    fn close_some(&mut self) -> Result<(), Error> {
        let mut to_close = Vec::new();
        for (token, owned) in &self.tokens {
            match &owned.mode {
                OwnedMode::Server(_) => continue,
                OwnedMode::Conn(conn) => {
                    // we've completed reading, and all the data has been consumed,
                    // and we've got nothing to write, or we can't write anymore
                    let nothing_more_to_write = conn.wants_shutdown && conn.write_buffer.is_empty();
                    if (conn.read_buffer.seen_eof && conn.read_buffer.is_empty())
                        && (nothing_more_to_write || conn.write_buffer.seen_eof)
                    {
                        info!("{} closing", token.0);
                        to_close.push(*token);
                    } else {
                        info!(
                            "{} not-closing: {} && {} && ({} || {})",
                            token.0,
                            conn.read_buffer.seen_eof,
                            conn.read_buffer.is_empty(),
                            nothing_more_to_write,
                            conn.write_buffer.seen_eof
                        );
                    }
                }
            }
        }

        for close in to_close {
            drop(self.tokens.remove(&close).expect("it was just there"));
        }

        Ok(())
    }

    fn reregister(&mut self) -> Result<(), Error> {
        for (token, owned) in &self.tokens {
            match &owned.mode {
                OwnedMode::Server(_) => continue,
                OwnedMode::Conn(conn) => {
                    let mut interest = Ready::empty();

                    if conn.read_buffer.has_space() {
                        interest |= Ready::readable();
                    }

                    if !conn.write_buffer.is_empty() {
                        interest |= Ready::writable();
                    }

                    self.poll
                        .reregister(&conn.inner, *token, interest, PollOpt::edge())?;
                }
            }
        }

        Ok(())
    }

    fn fill(&mut self) -> Result<(), Error> {
        self.close_some()?;

        self.reregister()?;

        let mut events = Events::with_capacity(32);
        self.poll.poll(&mut events, None)?;
        for ev in events {
            if COMMANDS_TOKEN == ev.token() {
                while let Ok(_) = self.channel.recv.try_recv() {
                    unimplemented!("commands")
                }
                continue;
            }

            let us: &mut Owned = match self.tokens.get_mut(&ev.token()) {
                Some(us) => us,
                None => continue,
            };

            info!("{} woke", ev.token().0);

            // TODO: outrageous duplication
            let mut woke = Vec::new();

            match us.mode {
                OwnedMode::Server(ref server) => {
                    let (sock, addr) = match block_to_none(server.inner.accept())? {
                        Some(o) => o,
                        None => continue,
                    };
                    let new = self.bump_token();
                    woke.push(Event::NewConnection(new));
                    self.poll
                        .register(&sock, new, Ready::readable(), PollOpt::edge())?;
                    self.tokens.insert(
                        new,
                        Owned {
                            token: new,
                            mode: OwnedMode::Conn(Conn {
                                inner: sock,
                                read_buffer: Stream::default(),
                                write_buffer: Stream::default(),
                                wants_shutdown: false,
                            }),
                        },
                    );
                }
                OwnedMode::Conn(ref mut conn) => {
                    let mut buf = [0u8; BUF_SIZE];
                    loop {
                        if conn.read_buffer.seen_eof {
                            break;
                        }

                        match block_to_none(conn.inner.read(&mut buf))? {
                            Some(0) => {
                                conn.read_buffer.seen_eof = true;
                                woke.push(Event::Done(ev.token(), Direction::Read));
                                break;
                            }

                            Some(r) => {
                                conn.read_buffer.inner.extend_from_slice(&buf[..r]);
                                woke.push(Event::Data(ev.token()));
                            }

                            None => break,
                        }
                    }

                    while !conn.write_buffer.seen_eof && !conn.write_buffer.inner.is_empty() {
                        let w = match block_to_none(conn.inner.write(&conn.write_buffer.inner))? {
                            Some(0) => {
                                conn.write_buffer.seen_eof = true;
                                woke.push(Event::Done(ev.token(), Direction::Write));
                                break;
                            }
                            Some(w) => w,
                            None => break,
                        };

                        drop(conn.write_buffer.inner.drain(..w));

                        woke.push(Event::Data(ev.token()));
                    }
                }
            }

            self.events.extend(woke);
        }

        Ok(())
    }
}

fn block_to_none<T>(res: Result<T, io::Error>) -> Result<Option<T>, io::Error> {
    match res {
        Ok(o) => Ok(Some(o)),
        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(e),
    }
}
