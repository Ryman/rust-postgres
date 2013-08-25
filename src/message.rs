use std::str;
use std::rt::io::{Decorator, Reader, Writer};
use std::rt::io::extensions::{ReaderUtil, ReaderByteConversions,
                              WriterByteConversions};
use std::rt::io::mem::{MemWriter, MemReader};
use std::hashmap::HashMap;
use std::sys;
use std::vec;

pub static PROTOCOL_VERSION: i32 = 0x0003_0000;

#[deriving(ToStr)]
pub enum BackendMessage {
    AuthenticationOk,
    BackendKeyData(i32, i32),
    BindComplete,
    CloseComplete,
    CommandComplete(~str),
    DataRow(~[Option<~[u8]>]),
    EmptyQueryResponse,
    ErrorResponse(HashMap<u8, ~str>),
    NoData,
    NoticeResponse(HashMap<u8, ~str>),
    ParameterDescription(~[i32]),
    ParameterStatus(~str, ~str),
    ParseComplete,
    ReadyForQuery(u8),
    RowDescription(~[RowDescriptionEntry])
}

#[deriving(ToStr)]
pub struct RowDescriptionEntry {
    name: ~str,
    table_oid: i32,
    column_id: i16,
    type_oid: i32,
    type_size: i16,
    type_modifier: i32,
    format: i16
}

pub enum FrontendMessage<'self> {
    /// portal, stmt, formats, values, result formats
    Bind(&'self str, &'self str, &'self [i16], &'self [Option<~[u8]>],
         &'self [i16]),
    Close(u8, &'self str),
    Describe(u8, &'self str),
    Execute(&'self str, i32),
    /// name, query, parameter types
    Parse(&'self str, &'self str, &'self [i32]),
    Query(&'self str),
    StartupMessage(HashMap<&'self str, &'self str>),
    Sync,
    Terminate
}

trait WriteString {
    fn write_string(&mut self, s: &str);
}

impl<W: Writer> WriteString for W {
    fn write_string(&mut self, s: &str) {
        self.write(s.as_bytes());
        self.write_u8_(0);
    }
}

pub trait WriteMessage {
    fn write_message(&mut self, &FrontendMessage);
}

impl<W: Writer> WriteMessage for W {
    fn write_message(&mut self, message: &FrontendMessage) {
        debug!("Writing message %?", message);
        let mut buf = MemWriter::new();
        let mut ident = None;

        match *message {
            Bind(portal, stmt, formats, values, result_formats) => {
                ident = Some('B');
                buf.write_string(portal);
                buf.write_string(stmt);

                buf.write_be_i16_(formats.len() as i16);
                for format in formats.iter() {
                    buf.write_be_i16_(*format);
                }

                buf.write_be_i16_(values.len() as i16);
                for value in values.iter() {
                    match *value {
                        None => {
                            buf.write_be_i32_(-1);
                        }
                        Some(ref value) => {
                            buf.write_be_i32_(value.len() as i32);
                            buf.write(*value);
                        }
                    }
                }

                buf.write_be_i16_(result_formats.len() as i16);
                for format in result_formats.iter() {
                    buf.write_be_i16_(*format);
                }
            }
            Close(variant, name) => {
                ident = Some('C');
                buf.write_u8_(variant);
                buf.write_string(name);
            }
            Describe(variant, name) => {
                ident = Some('D');
                buf.write_u8_(variant);
                buf.write_string(name);
            }
            Execute(name, num_rows) => {
                ident = Some('E');
                buf.write_string(name);
                buf.write_be_i32_(num_rows);
            }
            Parse(name, query, param_types) => {
                ident = Some('P');
                buf.write_string(name);
                buf.write_string(query);
                buf.write_be_i16_(param_types.len() as i16);
                for ty in param_types.iter() {
                    buf.write_be_i32_(*ty);
                }
            }
            Query(query) => {
                ident = Some('Q');
                buf.write_string(query);
            }
            StartupMessage(ref params) => {
                buf.write_be_i32_(PROTOCOL_VERSION);
                for (k, v) in params.iter() {
                    buf.write_string(*k);
                    buf.write_string(*v);
                }
                buf.write_u8_(0);
            }
            Sync => {
                ident = Some('S');
            }
            Terminate => {
                ident = Some('X');
            }
        }

        match ident {
            Some(ident) => self.write_u8_(ident as u8),
            None => ()
        }

        // add size of length value
        self.write_be_i32_((buf.inner_ref().len() + sys::size_of::<i32>())
                           as i32);
        self.write(buf.inner());
    }
}

trait ReadString {
    fn read_string(&mut self) -> ~str;
}

impl<R: Reader> ReadString for R {
    fn read_string(&mut self) -> ~str {
        let mut buf = ~[];
        loop {
            let byte = self.read_u8_();
            if byte == 0 {
                break;
            }

            buf.push(byte);
        }

        str::from_bytes_owned(buf)
    }
}

pub trait ReadMessage {
    fn read_message(&mut self) -> BackendMessage;
}

impl<R: Reader> ReadMessage for R {
    fn read_message(&mut self) -> BackendMessage {
        debug!("Reading message");

        let ident = self.read_u8_();
        // subtract size of length value
        let len = self.read_be_i32_() as uint - sys::size_of::<i32>();
        let mut buf = MemReader::new(self.read_bytes(len));

        let ret = match ident as char {
            '1' => ParseComplete,
            '2' => BindComplete,
            '3' => CloseComplete,
            'C' => CommandComplete(buf.read_string()),
            'D' => read_data_row(&mut buf),
            'E' => ErrorResponse(read_hash(&mut buf)),
            'I' => EmptyQueryResponse,
            'K' => BackendKeyData(buf.read_be_i32_(), buf.read_be_i32_()),
            'n' => NoData,
            'N' => NoticeResponse(read_hash(&mut buf)),
            'R' => read_auth_message(&mut buf),
            'S' => ParameterStatus(buf.read_string(), buf.read_string()),
            't' => read_parameter_description(&mut buf),
            'T' => read_row_description(&mut buf),
            'Z' => ReadyForQuery(buf.read_u8_()),
            ident => fail!("Unknown message identifier `%c`", ident)
        };
        assert!(buf.eof());
        debug!("Read message %?", ret);
        ret
    }
}

fn read_hash(buf: &mut MemReader) -> HashMap<u8, ~str> {
    let mut fields = HashMap::new();
    loop {
        let ty = buf.read_u8_();
        if ty == 0 {
            break;
        }

        fields.insert(ty, buf.read_string());
    }

    fields
}

fn read_data_row(buf: &mut MemReader) -> BackendMessage {
    let len = buf.read_be_i16_() as uint;
    let mut values = vec::with_capacity(len);

    do len.times() {
        let val = match buf.read_be_i32_() {
            -1 => None,
            len => Some(buf.read_bytes(len as uint))
        };
        values.push(val);
    }

    DataRow(values)
}

fn read_auth_message(buf: &mut MemReader) -> BackendMessage {
    match buf.read_be_i32_() {
        0 => AuthenticationOk,
        val => fail!("Unknown Authentication identifier `%?`", val)
    }
}

fn read_parameter_description(buf: &mut MemReader) -> BackendMessage {
    let len = buf.read_be_i16_() as uint;
    let mut types = vec::with_capacity(len);

    do len.times() {
        types.push(buf.read_be_i32_());
    }

    ParameterDescription(types)
}

fn read_row_description(buf: &mut MemReader) -> BackendMessage {
    let len = buf.read_be_i16_() as uint;
    let mut types = vec::with_capacity(len);

    do len.times() {
        types.push(RowDescriptionEntry {
            name: buf.read_string(),
            table_oid: buf.read_be_i32_(),
            column_id: buf.read_be_i16_(),
            type_oid: buf.read_be_i32_(),
            type_size: buf.read_be_i16_(),
            type_modifier: buf.read_be_i32_(),
            format: buf.read_be_i16_()
        });
    }

    RowDescription(types)
}