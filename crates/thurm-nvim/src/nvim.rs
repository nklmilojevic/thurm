//! Neovim's msgpack-RPC: `[0, id, method, params]` requests, `[1, id, error, result]`
//! responses and `[2, method, params]` notifications, over its `--listen` socket.

use std::io::{self, BufReader, BufWriter, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use rmpv::Value;

/// A message from Neovim.
#[derive(Debug)]
pub enum Message {
    Response {
        id: u32,
        result: Result<Value, Value>,
    },
    Notification {
        method: String,
        params: Vec<Value>,
    },
    /// Neovim asking us (`rpcrequest`): we serve none, and answer with an error.
    Request {
        id: u32,
        method: String,
    },
}

/// The writing half: requests and notifications to Neovim. Buffered: call [`Nvim::flush`].
pub struct Nvim {
    out: BufWriter<UnixStream>,
    next_id: u32,
}

impl Nvim {
    /// Connects to Neovim's socket, and returns the reading half for [`read_message`].
    pub fn connect(path: &Path) -> io::Result<(Nvim, BufReader<UnixStream>)> {
        let stream = UnixStream::connect(path)?;
        let reader = BufReader::with_capacity(64 * 1024, stream.try_clone()?);
        Ok((Nvim::new(stream), reader))
    }

    pub fn new(stream: UnixStream) -> Nvim {
        Nvim {
            out: BufWriter::with_capacity(64 * 1024, stream),
            next_id: 1,
        }
    }

    /// Sends a request; its answer comes back as a [`Message::Response`] with the id returned.
    pub fn request(&mut self, method: &str, params: Vec<Value>) -> io::Result<u32> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let msg = Value::Array(vec![
            Value::from(0),
            Value::from(id),
            Value::from(method),
            Value::Array(params),
        ]);
        rmpv::encode::write_value(&mut self.out, &msg).map_err(io::Error::other)?;
        Ok(id)
    }

    /// Answers a request from Neovim with an error.
    pub fn refuse(&mut self, id: u32, why: &str) -> io::Result<()> {
        let msg = Value::Array(vec![
            Value::from(1),
            Value::from(id),
            Value::from(why),
            Value::Nil,
        ]);
        rmpv::encode::write_value(&mut self.out, &msg).map_err(io::Error::other)
    }

    pub fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

/// The next message from Neovim; `None` when it closed the connection.
pub fn read_message(r: &mut impl Read) -> io::Result<Option<Message>> {
    let value = match rmpv::decode::read_value(r) {
        Ok(v) => v,
        Err(rmpv::decode::Error::InvalidMarkerRead(e))
            if e.kind() == io::ErrorKind::UnexpectedEof =>
        {
            return Ok(None);
        }
        Err(e) => return Err(io::Error::other(e)),
    };
    parse(value).map(Some)
}

fn parse(value: Value) -> io::Result<Message> {
    let bad = || io::Error::new(io::ErrorKind::InvalidData, "not a msgpack-RPC message");
    let Value::Array(mut parts) = value else {
        return Err(bad());
    };
    match parts.first().and_then(Value::as_u64) {
        Some(1) if parts.len() == 4 => {
            let result = parts.pop().unwrap_or(Value::Nil);
            let error = parts.pop().unwrap_or(Value::Nil);
            let id = parts[1].as_u64().ok_or_else(bad)? as u32;
            Ok(Message::Response {
                id,
                result: if error.is_nil() {
                    Ok(result)
                } else {
                    Err(error)
                },
            })
        }
        Some(2) if parts.len() == 3 => {
            let params = match parts.pop() {
                Some(Value::Array(p)) => p,
                _ => Vec::new(),
            };
            let method = parts[1].as_str().ok_or_else(bad)?.to_owned();
            Ok(Message::Notification { method, params })
        }
        Some(0) if parts.len() == 4 => Ok(Message::Request {
            id: parts[1].as_u64().ok_or_else(bad)? as u32,
            method: parts[2].as_str().unwrap_or("").to_owned(),
        }),
        _ => Err(bad()),
    }
}

/// A Neovim handle (window, buffer, tab page): an ext value holding an integer, or a plain
/// integer.
pub fn handle(v: &Value) -> Option<i64> {
    match v {
        Value::Ext(_, data) => rmpv::decode::read_value(&mut &data[..]).ok()?.as_i64(),
        v => v.as_i64(),
    }
}

/// Error text from a Neovim error response (`[type, message]`).
pub fn error_text(v: &Value) -> String {
    match v {
        Value::Array(a) => a
            .get(1)
            .and_then(Value::as_str)
            .unwrap_or("error")
            .to_owned(),
        v => v.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(v: Value) -> Message {
        let mut buf = Vec::new();
        rmpv::encode::write_value(&mut buf, &v).unwrap();
        read_message(&mut &buf[..]).unwrap().unwrap()
    }

    #[test]
    fn messages_parse() {
        let Message::Response { id, result } = roundtrip(Value::Array(vec![
            1.into(),
            7.into(),
            Value::Nil,
            "ok".into(),
        ])) else {
            panic!()
        };
        assert_eq!(
            (id, result.unwrap().as_str().map(str::to_owned)),
            (7, Some("ok".into()))
        );

        let Message::Response { result, .. } = roundtrip(Value::Array(vec![
            1.into(),
            8.into(),
            Value::Array(vec![0.into(), "E5108: boom".into()]),
            Value::Nil,
        ])) else {
            panic!()
        };
        assert_eq!(error_text(&result.unwrap_err()), "E5108: boom");

        let Message::Notification { method, params } = roundtrip(Value::Array(vec![
            2.into(),
            "redraw".into(),
            Value::Array(vec![Value::Array(vec!["flush".into()])]),
        ])) else {
            panic!()
        };
        assert_eq!((method.as_str(), params.len()), ("redraw", 1));

        assert!(read_message(&mut &[][..]).unwrap().is_none());
    }

    #[test]
    fn handles_are_ext_integers() {
        let mut data = Vec::new();
        rmpv::encode::write_value(&mut data, &Value::from(1001)).unwrap();
        assert_eq!(handle(&Value::Ext(1, data)), Some(1001));
        assert_eq!(handle(&Value::from(3)), Some(3));
    }

    #[test]
    fn requests_are_written() {
        let (a, b) = UnixStream::pair().unwrap();
        let mut nvim = Nvim::new(a);
        let id = nvim.request("nvim_input", vec!["<CR>".into()]).unwrap();
        nvim.flush().unwrap();
        let v = rmpv::decode::read_value(&mut &b).unwrap();
        assert_eq!(
            v,
            Value::Array(vec![
                0.into(),
                id.into(),
                "nvim_input".into(),
                Value::Array(vec!["<CR>".into()])
            ])
        );
    }
}
