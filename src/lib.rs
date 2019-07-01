use std::collections::{HashMap, VecDeque};
use std::io;
use std::io::Read;
use std::io::Write;
use std::mem;
use std::net::SocketAddr;

use failure::Error;
use log::debug;
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
}

struct Stream {
    state: StreamState,
}

enum StreamState {
    Normal { buf: Vec<u8>, wanted: usize },
    Draining { buf: Vec<u8> },
    AwaitingConfirmation,
    Done,
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
    fn read_interest(&self) -> bool {
        match &self.state {
            StreamState::Normal { buf, wanted } => buf.len() < *wanted,
            StreamState::AwaitingConfirmation => true,
            StreamState::Draining { .. } | StreamState::Done => false,
        }
    }

    fn do_read(&self) -> bool {
        match self.state {
            StreamState::Normal { .. } | StreamState::AwaitingConfirmation => true,
            StreamState::Draining { .. } | StreamState::Done => false,
        }
    }

    fn write_interest(&self) -> bool {
        match &self.state {
            StreamState::Normal { buf, .. } | StreamState::Draining { buf } => !buf.is_empty(),
            StreamState::AwaitingConfirmation => true,
            StreamState::Done => false,
        }
    }

    fn do_write(&self) -> bool {
        match &self.state {
            StreamState::Normal { buf, .. } | StreamState::Draining { buf } => !buf.is_empty(),
            StreamState::AwaitingConfirmation => true,
            StreamState::Done => false,
        }
    }

    fn is_done(&self) -> bool {
        match self.state {
            StreamState::Done => true,
            _ => false,
        }
    }

    fn buf(&self) -> Option<&[u8]> {
        match &self.state {
            StreamState::Normal { buf, .. } | StreamState::Draining { buf } => Some(buf),
            StreamState::AwaitingConfirmation | StreamState::Done => None,
        }
    }

    fn buf_mut(&mut self) -> Option<&mut Vec<u8>> {
        match &mut self.state {
            StreamState::Normal { buf, .. } | StreamState::Draining { buf } => Some(buf),
            StreamState::AwaitingConfirmation | StreamState::Done => None,
        }
    }

    fn become_truncating_close(&mut self) {
        debug!("become-truncating-close");
        self.state = match self.state {
            StreamState::Normal { .. } | StreamState::Draining { .. } => {
                StreamState::AwaitingConfirmation
            }
            StreamState::AwaitingConfirmation | StreamState::Done => panic!("invalid state"),
        };
    }

    fn become_draining_close(&mut self) {
        debug!("become-draining-close");
        let borrow_checker_avoid = StreamState::AwaitingConfirmation;
        let mut temp_state = mem::replace(&mut self.state, borrow_checker_avoid);
        self.state = match temp_state {
            StreamState::Normal { buf, .. } => StreamState::Draining { buf },
            StreamState::Draining { .. }
            | StreamState::AwaitingConfirmation
            | StreamState::Done => panic!("invalid state"),
        };
    }

    fn totes_done(&mut self) {
        debug!("totes-done");
        self.state = StreamState::Done;
    }
}

impl Default for Stream {
    fn default() -> Stream {
        Stream {
            state: StreamState::Normal {
                buf: Vec::new(),
                wanted: 8 * 1024,
            },
        }
    }
}

impl<'n> Io<'n> {
    fn as_conn(&self) -> &Conn {
        match self
            .inner
            .tokens
            .get(&self.token)
            .expect("io for non-conn")
            .mode
        {
            OwnedMode::Conn(ref conn) => conn,
            _ => unreachable!("must be a conn"),
        }
    }

    fn as_conn_mut(&mut self) -> &mut Conn {
        match self
            .inner
            .tokens
            .get_mut(&self.token)
            .expect("io for non-conn")
            .mode
        {
            OwnedMode::Conn(ref mut conn) => conn,
            _ => unreachable!("must be a conn"),
        }
    }

    pub fn buf(&self) -> &[u8] {
        self.as_conn()
            .read_buffer
            .buf()
            .expect("TODO: read buffer closed")
    }

    pub fn consume(&mut self, len: usize) {
        drop(
            self.as_conn_mut()
                .read_buffer
                .buf_mut()
                .expect("TODO: read buffer closed")
                .drain(..len),
        )
    }

    pub fn write(&mut self, data: &[u8]) {
        self.as_conn_mut()
            .write_buffer
            .buf_mut()
            .expect("TODO: write buffer closed")
            .extend_from_slice(data)
    }

    pub fn close(&mut self) -> () {
        let conn = self.as_conn_mut();
        conn.read_buffer.become_truncating_close();
        conn.write_buffer.become_draining_close();
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
                    if conn.read_buffer.is_done() && conn.write_buffer.is_done() {
                        info!("{} closing", token.0);
                        to_close.push(*token);
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

                    if conn.read_buffer.read_interest() {
                        interest |= Ready::readable();
                    }

                    if !conn.write_buffer.write_interest() {
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
                            }),
                        },
                    );
                }
                OwnedMode::Conn(ref mut conn) => shunt_io(&mut woke, conn, ev.token()),
            }

            self.events.extend(woke);
        }

        Ok(())
    }
}

fn shunt_io(woke: &mut Vec<Event>, conn: &mut Conn, token: Token) {
    while conn.read_buffer.do_read() && do_a_read(woke, conn, token) {}
    while conn.write_buffer.do_write() && do_a_write(woke, conn, token) {}
}

fn do_a_read(woke: &mut Vec<Event>, conn: &mut Conn, token: Token) -> bool {
    let mut buf = [0u8; BUF_SIZE];
    match conn.inner.read(&mut buf) {
        Ok(0) => {
            conn.read_buffer.become_draining_close();
            woke.push(Event::Done(token, Direction::Read));
            false
        }

        Ok(r) => {
            conn.read_buffer
                .buf_mut()
                .expect("TODO: read completed on non-buffer")
                .extend_from_slice(&buf[..r]);
            woke.push(Event::Data(token));
            true
        }

        Err(ref e) if io::ErrorKind::WouldBlock == e.kind() => false,

        Err(e) => {
            info!("{} read-err {:?}", token.0, e);
            conn.read_buffer.become_truncating_close();
            true
        }
    }
}

fn do_a_write(woke: &mut Vec<Event>, conn: &mut Conn, token: Token) -> bool {
    match conn.inner.write(
        conn.write_buffer
            .buf()
            .expect("asked to write, should be able to see data to write"),
    ) {
        Ok(0) => {
            info!("{} write-eof", token.0);
            conn.write_buffer.totes_done();
            woke.push(Event::Done(token, Direction::Write));
            false
        }
        Ok(w) => {
            drop(
                conn.write_buffer
                    .buf_mut()
                    .expect("wrote data, should be able to discard it")
                    .drain(..w),
            );
            woke.push(Event::Data(token));
            true
        }

        Err(ref e) if io::ErrorKind::WouldBlock == e.kind() => false,

        Err(e) => {
            info!("{} write-err {:?}", token.0, e);
            conn.write_buffer.totes_done();
            false
        }
    }
}

fn block_to_none<T>(res: Result<T, io::Error>) -> Result<Option<T>, io::Error> {
    match res {
        Ok(o) => Ok(Some(o)),
        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(e),
    }
}
