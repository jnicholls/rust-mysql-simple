use std::io;
use std::io::Read as NewRead;
use std::io::Write as NewWrite;
use std::net;
use std::fmt;

use super::value::Value;
use super::value::Value::{NULL, Int, UInt, Float, Bytes, Date, Time};
use super::consts;
use super::consts::Command;
use super::consts::ColumnType;
use super::error::MyError::MyDriverError;
use super::error::DriverError::PacketTooLarge;
use super::error::DriverError::PacketOutOfSync;
use super::error::MyResult;

#[cfg(feature = "openssl")]
use openssl::{ssl, x509};
use byteorder::ByteOrder;
use byteorder::ReadBytesExt;
use byteorder::WriteBytesExt;
use byteorder::LittleEndian as LE;
#[cfg(feature = "socket")]
use unix_socket as us;
#[cfg(feature = "pipe")]
use named_pipe as np;

pub trait Read: ReadBytesExt {
    fn read_lenenc_int(&mut self) -> io::Result<u64> {
        let head_byte = try!(self.read_u8());
        let length = match head_byte {
            0xfc => 2,
            0xfd => 3,
            0xfe => 8,
            x => return Ok(x as u64),
        };
        let out = try!(self.read_uint::<LE>(length));
        Ok(out)
    }

    fn read_lenenc_bytes(&mut self) -> io::Result<Vec<u8>> {
        let len = try!(self.read_lenenc_int());
        let mut out = Vec::with_capacity(len as usize);
        let count = if len > 0 {
            try!(self.take(len).read_to_end(&mut out))
        } else {
            0
        };
        if count as u64 == len {
            Ok(out)
        } else {
            Err(io::Error::new(io::ErrorKind::Other,
                               "Unexpected EOF while reading length encoded string"))
        }
    }

    fn read_to_null(&mut self) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut chars = self.bytes();
        while let Some(c) = chars.next() {
            let c = try!(c);
            if c == 0u8 {
                break;
            }
            out.push(c);
        }
        Ok(out)
    }

    fn read_bin_value(&mut self, col_type: consts::ColumnType, unsigned: bool) -> io::Result<Value> {
        match col_type {
            ColumnType::MYSQL_TYPE_STRING |
            ColumnType::MYSQL_TYPE_VAR_STRING |
            ColumnType::MYSQL_TYPE_BLOB |
            ColumnType::MYSQL_TYPE_TINY_BLOB |
            ColumnType::MYSQL_TYPE_MEDIUM_BLOB |
            ColumnType::MYSQL_TYPE_LONG_BLOB |
            ColumnType::MYSQL_TYPE_SET |
            ColumnType::MYSQL_TYPE_ENUM |
            ColumnType::MYSQL_TYPE_DECIMAL |
            ColumnType::MYSQL_TYPE_VARCHAR |
            ColumnType::MYSQL_TYPE_BIT |
            ColumnType::MYSQL_TYPE_NEWDECIMAL |
            ColumnType::MYSQL_TYPE_GEOMETRY => {
                Ok(Bytes(try!(self.read_lenenc_bytes())))
            },
            ColumnType::MYSQL_TYPE_TINY => {
                if unsigned {
                    Ok(Int(try!(self.read_u8()) as i64))
                } else {
                    Ok(Int(try!(self.read_i8()) as i64))
                }
            },
            ColumnType::MYSQL_TYPE_SHORT |
            ColumnType::MYSQL_TYPE_YEAR => {
                if unsigned {
                    Ok(Int(try!(self.read_u16::<LE>()) as i64))
                } else {
                    Ok(Int(try!(self.read_i16::<LE>()) as i64))
                }
            },
            ColumnType::MYSQL_TYPE_LONG |
            ColumnType::MYSQL_TYPE_INT24 => {
                if unsigned {
                    Ok(Int(try!(self.read_u32::<LE>()) as i64))
                } else {
                    Ok(Int(try!(self.read_i32::<LE>()) as i64))
                }
            },
            ColumnType::MYSQL_TYPE_LONGLONG => {
                if unsigned {
                    Ok(UInt(try!(self.read_u64::<LE>())))
                } else {
                    Ok(Int(try!(self.read_i64::<LE>())))
                }
            },
            ColumnType::MYSQL_TYPE_FLOAT => {
                Ok(Float(try!(self.read_f32::<LE>()) as f64))
            },
            ColumnType::MYSQL_TYPE_DOUBLE => {
                Ok(Float(try!(self.read_f64::<LE>())))
            },
            ColumnType::MYSQL_TYPE_TIMESTAMP |
            ColumnType::MYSQL_TYPE_DATE |
            ColumnType::MYSQL_TYPE_DATETIME => {
                let len = try!(self.read_u8());
                let mut year = 0u16;
                let mut month = 0u8;
                let mut day = 0u8;
                let mut hour = 0u8;
                let mut minute = 0u8;
                let mut second = 0u8;
                let mut micro_second = 0u32;
                if len >= 4u8 {
                    year = try!(self.read_u16::<LE>());
                    month = try!(self.read_u8());
                    day = try!(self.read_u8());
                }
                if len >= 7u8 {
                    hour = try!(self.read_u8());
                    minute = try!(self.read_u8());
                    second = try!(self.read_u8());
                }
                if len == 11u8 {
                    micro_second = try!(self.read_u32::<LE>());
                }
                Ok(Date(year, month, day, hour, minute, second, micro_second))
            },
            ColumnType::MYSQL_TYPE_TIME => {
                let len = try!(self.read_u8());
                let mut is_negative = false;
                let mut days = 0u32;
                let mut hours = 0u8;
                let mut minutes = 0u8;
                let mut seconds = 0u8;
                let mut micro_seconds = 0u32;
                if len >= 8u8 {
                    is_negative = try!(self.read_u8()) == 1u8;
                    days = try!(self.read_u32::<LE>());
                    hours = try!(self.read_u8());
                    minutes = try!(self.read_u8());
                    seconds = try!(self.read_u8());
                }
                if len == 12u8 {
                    micro_seconds = try!(self.read_u32::<LE>());
                }
                Ok(Time(is_negative, days, hours, minutes, seconds, micro_seconds))
            },
            _ => Ok(NULL),
        }
    }

    /// Reads mysql packet payload returns it with new seq_id value.
    fn read_packet(&mut self, mut seq_id: u8) -> MyResult<(Vec<u8>, u8)> {
        use std::io::ErrorKind::Other;
        let mut output = Vec::new();
        loop {
            let payload_len = try!(self.read_uint::<LE>(3)) as usize;
            println!("payload_len: {}", payload_len);
            let srv_seq_id = try!(self.read_u8());
            println!("srv_seq_id: {}", srv_seq_id);
            if srv_seq_id != seq_id {
                return Err(MyDriverError(PacketOutOfSync));
            }
            seq_id = seq_id.wrapping_add(1);
            if payload_len == consts::MAX_PAYLOAD_LEN {
                output.reserve(consts::MAX_PAYLOAD_LEN);
                let mut chunk = self.take(consts::MAX_PAYLOAD_LEN as u64);
                let count = try!(chunk.read_to_end(&mut output));
                if count != consts::MAX_PAYLOAD_LEN {
                    return Err(io::Error::new(Other, "Unexpected EOF while reading packet").into())
                }
            } else if payload_len == 0 {
                break;
            } else {
                output.reserve(payload_len);
                let mut chunk = self.take(payload_len as u64);
                let count = try!(chunk.read_to_end(&mut output));
                println!("count: {} -- output: {:?}", count, output);
                if count != payload_len {
                    return Err(io::Error::new(Other, "Unexpected EOF while reading packet").into())
                }
                break;
            }
        }
        Ok((output, seq_id))
    }
}

impl<T: ReadBytesExt> Read for T {}

pub trait Write: WriteBytesExt {
    fn write_le_uint_n(&mut self, x: u64, len: usize) -> io::Result<()> {
        let mut buf = [0u8; 8];
        let mut offset = 0;
        while offset < len {
            buf[offset] = (((0xFF << (offset * 8)) & x) >> (offset * 8)) as u8;
            offset += 1;
        }
        NewWrite::write_all(self, &buf[..len])
    }

    fn write_lenenc_int(&mut self, x: u64) -> io::Result<()> {
        if x < 251 {
            try!(self.write_u8(x as u8));
            Ok(())
        } else if x < 65_536 {
            try!(self.write_u8(0xFC));
            self.write_le_uint_n(x, 2)
        } else if x < 16_777_216 {
            try!(self.write_u8(0xFD));
            self.write_le_uint_n(x, 3)
        } else {
            try!(self.write_u8(0xFE));
            self.write_le_uint_n(x, 8)
        }
    }

    fn write_lenenc_bytes(&mut self, bytes: &[u8]) -> io::Result<()> {
        try!(self.write_lenenc_int(bytes.len() as u64));
        self.write_all(bytes)
    }

    fn write_packet(&mut self, data: &[u8], mut seq_id: u8, max_allowed_packet: usize) -> MyResult<u8> {
        if data.len() > max_allowed_packet &&
           max_allowed_packet < consts::MAX_PAYLOAD_LEN {
            return Err(MyDriverError(PacketTooLarge));
        }
        if data.len() == 0 {
            try!(self.write_all(&[0u8, 0u8, 0u8, seq_id]));
            return Ok(seq_id + 1);
        }
        let mut last_was_max = false;
        for chunk in data.chunks(consts::MAX_PAYLOAD_LEN) {
            let chunk_len = chunk.len();
            let mut writer = Vec::with_capacity(4 + chunk_len);
            try!(writer.write_le_uint_n(chunk_len as u64, 3));
            try!(writer.write_u8(seq_id));
            try!(writer.write_all(chunk));
            try!(self.write_all(&writer[..]));
            last_was_max = chunk_len == consts::MAX_PAYLOAD_LEN;
            seq_id += 1;
        }
        if last_was_max {
            try!(self.write_all(&[0u8, 0u8, 0u8, seq_id]));
            seq_id += 1;
        }
        Ok(seq_id)
    }
}

impl<T: WriteBytesExt> Write for T {}

#[derive(Debug)]
pub enum Stream {
    #[cfg(feature = "socket")]
    UnixStream(us::UnixStream),
    #[cfg(feature = "pipe")]
    PipeStream(np::PipeClient),
    TcpStream(Option<TcpStream>),
}

#[cfg(feature = "openssl")]
impl Stream {
    pub fn is_insecure(&self) -> bool {
        match self {
            &Stream::TcpStream(Some(TcpStream::Insecure(_))) => true,
            _ => false,
        }
    }
    pub fn make_secure(mut self,
                       verify_peer: bool,
                       ssl_opts: &Option<(::std::path::PathBuf,
                                          Option<(::std::path::PathBuf, ::std::path::PathBuf)>)>)
    -> MyResult<Stream>
    {
        if self.is_insecure() {
            let mut ctx = try!(ssl::SslContext::new(ssl::SslMethod::Tlsv1));
            let mode = if verify_peer {
                ssl::SSL_VERIFY_PEER
            } else {
                ssl::SSL_VERIFY_NONE
            };
            ctx.set_verify(mode, None);
            match *ssl_opts {
                Some((ref ca_cert, None)) => try!(ctx.set_CA_file(&ca_cert)),
                Some((ref ca_cert, Some((ref client_cert, ref client_key)))) => {
                    try!(ctx.set_CA_file(&ca_cert));
                    try!(ctx.set_certificate_file(&client_cert, x509::X509FileType::PEM));
                    try!(ctx.set_private_key_file(&client_key, x509::X509FileType::PEM));
                },
                _ => unreachable!(),
            }
            match self {
                Stream::TcpStream(ref mut opt_stream) if opt_stream.is_some() => {
                    let stream = opt_stream.take().unwrap();
                    match stream {
                        TcpStream::Insecure(stream) => {
                            let ssl_stream = try!(ssl::SslStream::connect(&ctx, stream));
                            Ok(Stream::TcpStream(Some(TcpStream::Secure(ssl_stream))))
                        },
                        _ => unreachable!(),
                    }

                },
                _ => unreachable!(),
            }
        } else {
            Ok(self)
        }
    }
}

impl Drop for Stream {
    fn drop(&mut self) {
        if let &mut Stream::TcpStream(None) = self {
            return;
        }
        let _ = self.write_packet(&[Command::COM_QUIT as u8], 0, consts::MAX_PAYLOAD_LEN);
        let _ = self.flush();
    }
}

impl io::Read for Stream {
    #[cfg(feature = "socket")]
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match *self {
            Stream::UnixStream(ref mut s) => s.read(buf),
            Stream::TcpStream(Some(ref mut s)) => s.read(buf),
            _ => panic!("Incomplete stream"),
        }
    }

    #[cfg(feature = "pipe")]
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match *self {
            Stream::PipeStream(ref mut s) => s.read(buf),
            Stream::TcpStream(Some(ref mut s)) => s.read(buf),
            _ => panic!("Incomplete stream"),
        }
    }

    #[cfg(all(not(feature = "pipe"), not(feature = "socket")))]
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match *self {
            Stream::TcpStream(Some(ref mut s)) => s.read(buf),
            _ => panic!("Incomplete stream"),
        }
    }
}

impl io::Write for Stream {
    #[cfg(feature = "socket")]
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match *self {
            Stream::UnixStream(ref mut s) => s.write(buf),
            Stream::TcpStream(Some(ref mut s)) => s.write(buf),
            _ => panic!("Incomplete stream"),
        }
    }

    #[cfg(feature = "socket")]
    fn flush(&mut self) -> io::Result<()> {
        match *self {
            Stream::UnixStream(ref mut s) => s.flush(),
            Stream::TcpStream(Some(ref mut s)) => s.flush(),
            _ => panic!("Incomplete stream"),
        }
    }

    #[cfg(feature = "pipe")]
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match *self {
            Stream::PipeStream(ref mut s) => s.write(buf),
            Stream::TcpStream(Some(ref mut s)) => s.write(buf),
            _ => panic!("Incomplete stream"),
        }
    }

    #[cfg(feature = "pipe")]
    fn flush(&mut self) -> io::Result<()> {
        match *self {
            Stream::PipeStream(ref mut s) => s.flush(),
            Stream::TcpStream(Some(ref mut s)) => s.flush(),
            _ => panic!("Incomplete stream"),
        }
    }

    #[cfg(all(not(feature = "pipe"), not(feature = "socket")))]
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match *self {
            Stream::TcpStream(Some(ref mut s)) => s.write(buf),
            _ => panic!("Incomplete stream"),
        }
    }

    #[cfg(all(not(feature = "pipe"), not(feature = "socket")))]
    fn flush(&mut self) -> io::Result<()> {
        match *self {
            Stream::TcpStream(Some(ref mut s)) => s.flush(),
            _ => panic!("Incomplete stream"),
        }
    }
}

pub enum TcpStream {
    #[cfg(feature = "openssl")]
    Secure(ssl::SslStream<net::TcpStream>),
    Insecure(net::TcpStream),
}

#[cfg(feature = "ssl")]
impl fmt::Debug for TcpStream {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            TcpStream::Secure(_) => write!(f, "Secure stream"),
            TcpStream::Insecure(_) => write!(f, "Insecure stream"),
        }
    }
}

#[cfg(not(feature = "ssl"))]
impl fmt::Debug for TcpStream {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            TcpStream::Insecure(_) => write!(f, "Insecure stream"),
        }
    }
}

#[cfg(feature = "ssl")]
impl io::Read for TcpStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match *self {
            TcpStream::Secure(ref mut s) => s.read(buf),
            TcpStream::Insecure(ref mut s) => s.read(buf),
        }
    }
}

#[cfg(not(feature = "ssl"))]
impl io::Read for TcpStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match *self {
            TcpStream::Insecure(ref mut s) => s.read(buf),
        }
    }
}

#[cfg(feature = "ssl")]
impl io::Write for TcpStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match *self {
            TcpStream::Secure(ref mut s) => s.write(buf),
            TcpStream::Insecure(ref mut s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match *self {
            TcpStream::Secure(ref mut s) => s.flush(),
            TcpStream::Insecure(ref mut s) => s.flush(),
        }
    }
}

#[cfg(not(feature = "ssl"))]
impl io::Write for TcpStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match *self {
            TcpStream::Insecure(ref mut s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match *self {
            TcpStream::Insecure(ref mut s) => s.flush(),
        }
    }
}
