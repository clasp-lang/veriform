//! Veriform message decoder

mod body;
mod header;
mod value;

use super::{Decodable, Event};
use crate::{
    error::Error,
    field::{Header, Tag, WireType},
};

/// Veriform decoder: streaming zero-copy pull parser which emits events based
/// on incoming data.
#[derive(Debug)]
pub struct Decoder {
    /// Current state of the decoder (or `None` if an error occurred)
    state: Option<State>,

    /// Last field tag that was decoded (to ensure monotonicity)
    last_tag: Option<Tag>,
}

impl Default for Decoder {
    fn default() -> Self {
        Self {
            state: Some(State::default()),
            last_tag: None,
        }
    }
}

impl Decoder {
    /// Create a new decoder in an initial state
    pub fn new() -> Self {
        Self::default()
    }

    /// Decode an expected field header, returning an error for anything else
    pub fn decode_header(&mut self, input: &mut &[u8]) -> Result<Header, Error> {
        if let Some(Event::FieldHeader(header)) = self.decode(input)? {
            Ok(header)
        } else {
            Err(Error::Decode)
        }
    }

    /// Decode an expected field header, skipping (in-order) unknown fields,
    /// and returning an error if the field is missing or unexpected
    pub fn decode_expected_header(
        &mut self,
        input: &mut &[u8],
        tag: Tag,
        wire_type: WireType,
    ) -> Result<(), Error> {
        let header = self.decode_header(input)?;

        // TODO(tarcieri): actually skip unknown fields
        if header.tag != tag {
            return Err(Error::Decode);
        }

        if header.wire_type != wire_type {
            return Err(Error::WireType);
        }

        Ok(())
    }

    /// Decode a length delimiter, expecting the given wire type
    fn decode_length_delimiter(
        &mut self,
        input: &mut &[u8],
        expected_type: WireType,
    ) -> Result<usize, Error> {
        debug_assert!(expected_type.is_length_delimited());

        if let Some(Event::LengthDelimiter { wire_type, length }) = self.decode(input)? {
            if wire_type == expected_type {
                return Ok(length);
            }
        }

        Err(Error::Decode)
    }
}

impl Decodable for Decoder {
    fn decode<'a>(&mut self, input: &mut &'a [u8]) -> Result<Option<Event<'a>>, Error> {
        if let Some(state) = self.state.take() {
            let (new_state, event) = state.decode(input, self.last_tag)?;

            if let Some(Event::FieldHeader(header)) = &event {
                self.last_tag = Some(header.tag);
            }

            self.state = Some(new_state);
            Ok(event)
        } else {
            Err(Error::Failed)
        }
    }

    fn decode_length_delimited_value<'a>(
        &mut self,
        input: &mut &'a [u8],
        expected_type: WireType,
    ) -> Result<&'a [u8], Error> {
        let length = self.decode_length_delimiter(input, expected_type)?;

        if let Some(Event::ValueChunk {
            wire_type,
            bytes,
            remaining,
        }) = self.decode(input)?
        {
            if wire_type == expected_type && remaining == 0 {
                debug_assert_eq!(length, bytes.len());
                return Ok(bytes);
            }
        }

        Err(Error::Decode)
    }
}

/// Decoder state machine
#[derive(Debug)]
enum State {
    /// Reading the initial `vint64` header on a field
    Header(header::Decoder),

    /// Reading the `vint64` value of a field (either value itself or length prefix)
    Value(value::Decoder),

    /// Reading the body of a variable-length field
    Body(body::Decoder),
}

impl State {
    /// Process the given input data, advancing the slice for the amount of
    /// data processed, and returning the new state.
    pub(super) fn decode<'a>(
        self,
        input: &mut &'a [u8],
        last_tag: Option<Tag>,
    ) -> Result<(Self, Option<Event<'a>>), Error> {
        match self {
            State::Header(header) => header.decode(input, last_tag),
            State::Value(value) => value.decode(input),
            State::Body(body) => body.decode(input),
        }
    }

    /// Get the new state to transition to based on a given event
    pub(super) fn transition(event: &Event<'_>) -> Self {
        match event {
            Event::FieldHeader(header) => value::Decoder::new(header.wire_type).into(),
            Event::Bool(_) | Event::UInt64(_) | Event::SInt64(_) => State::default(),
            Event::LengthDelimiter { wire_type, length } => {
                if *length > 0 {
                    body::Decoder::new(*wire_type, *length).into()
                } else {
                    State::default()
                }
            }
            Event::SequenceHeader { length, .. } => {
                if *length > 0 {
                    body::Decoder::new(WireType::Sequence, *length).into()
                } else {
                    State::default()
                }
            }
            Event::ValueChunk {
                wire_type,
                remaining,
                ..
            } => {
                if *remaining > 0 {
                    body::Decoder::new(*wire_type, *remaining).into()
                } else {
                    State::default()
                }
            }
        }
    }
}

impl Default for State {
    fn default() -> State {
        State::Header(Default::default())
    }
}

impl From<value::Decoder> for State {
    fn from(decoder: value::Decoder) -> State {
        State::Value(decoder)
    }
}

#[cfg(test)]
mod tests {
    use super::{Decodable, Decoder, WireType};
    use crate::error::Error;

    #[test]
    fn decode_false() {
        let input = [130, 10];
        let mut input_ref = &input[..];
        let mut decoder = Decoder::new();

        let header = decoder.decode_header(&mut input_ref).unwrap();
        assert_eq!(header.tag, 42);
        assert_eq!(header.wire_type, WireType::False);
    }

    #[test]
    fn decode_true() {
        let input = [198, 10];
        let mut input_ref = &input[..];
        let mut decoder = Decoder::new();

        let header = decoder.decode_header(&mut input_ref).unwrap();
        assert_eq!(header.tag, 43);
        assert_eq!(header.wire_type, WireType::True);
    }

    #[test]
    fn decode_uint64() {
        let input = [138, 10, 85];
        let mut input_ref = &input[..];
        let mut decoder = Decoder::new();

        let header = decoder.decode_header(&mut input_ref).unwrap();
        assert_eq!(header.tag, 42);
        assert_eq!(header.wire_type, WireType::UInt64);

        let value = decoder.decode_uint64(&mut input_ref).unwrap();
        assert_eq!(value, 42);
        assert!(input_ref.is_empty());
    }

    #[test]
    fn decode_sint64() {
        let input = [206, 10, 167];
        let mut input_ref = &input[..];
        let mut decoder = Decoder::new();

        let header = decoder.decode_header(&mut input_ref).unwrap();
        assert_eq!(header.tag, 43);
        assert_eq!(header.wire_type, WireType::SInt64);

        let value = decoder.decode_sint64(&mut input_ref).unwrap();
        assert_eq!(value, -42);
        assert!(input_ref.is_empty());
    }

    #[test]
    fn decode_bytes() {
        let input = [73, 11, 98, 121, 116, 101, 115];
        let mut input_ref = &input[..];
        let mut decoder = Decoder::new();

        let header = decoder.decode_header(&mut input_ref).unwrap();
        assert_eq!(header.tag, 2);
        assert_eq!(header.wire_type, WireType::Bytes);

        let bytes = decoder.decode_bytes(&mut input_ref).unwrap();
        assert_eq!(bytes, &[98, 121, 116, 101, 115]);
        assert!(input_ref.is_empty());
    }

    #[test]
    fn decode_string() {
        let input = [139, 7, 98, 97, 122];
        let mut input_ref = &input[..];
        let mut decoder = Decoder::new();

        let header = decoder.decode_header(&mut input_ref).unwrap();
        assert_eq!(header.tag, 4);
        assert_eq!(header.wire_type, WireType::String);

        let string = decoder.decode_string(&mut input_ref).unwrap();
        assert_eq!(string, "baz");
        assert!(input_ref.is_empty());
    }

    #[test]
    fn decode_message() {
        let input = [45, 5, 69, 7];
        let mut input_ref = &input[..];
        let mut decoder = Decoder::new();

        let header = decoder.decode_header(&mut input_ref).unwrap();
        assert_eq!(header.tag, 1);
        assert_eq!(header.wire_type, WireType::Message);

        let message = decoder.decode_message(&mut input_ref).unwrap();
        assert_eq!(message, &[69, 7]);
        assert!(input_ref.is_empty());
    }

    #[test]
    fn decode_multiple() {
        let input = [138, 10, 85, 206, 10, 167];
        let mut input_ref = &input[..];
        let mut decoder = Decoder::new();

        let header = decoder.decode_header(&mut input_ref).unwrap();
        assert_eq!(header.tag, 42);
        assert_eq!(header.wire_type, WireType::UInt64);

        let value = decoder.decode_uint64(&mut input_ref).unwrap();
        assert_eq!(value, 42);

        let header = decoder.decode_header(&mut input_ref).unwrap();
        assert_eq!(header.tag, 43);
        assert_eq!(header.wire_type, WireType::SInt64);

        let value = decoder.decode_sint64(&mut input_ref).unwrap();
        assert_eq!(value, -42);
        assert!(input_ref.is_empty());
    }

    #[test]
    fn decode_partial_field_header() {
        let input = [138, 10, 85];
        let mut decoder = Decoder::new();

        let mut input_ref = &input[..1];
        assert_eq!(decoder.decode(&mut input_ref).unwrap(), None);

        input_ref = &input[1..];
        let header = decoder.decode_header(&mut input_ref).unwrap();
        assert_eq!(header.tag, 42);
        assert_eq!(header.wire_type, WireType::UInt64);
    }

    #[test]
    fn decode_out_of_order() {
        let input = [206, 10, 167, 138, 10, 85];
        let mut input_ref = &input[..];
        let mut decoder = Decoder::new();

        let header = decoder.decode_header(&mut input_ref).unwrap();
        assert_eq!(header.tag, 43);
        assert_eq!(header.wire_type, WireType::SInt64);

        let value = decoder.decode_sint64(&mut input_ref).unwrap();
        assert_eq!(value, -42);

        let error = decoder.decode(&mut input_ref).err().unwrap();
        assert_eq!(error, Error::Order { tag: 42 })
    }
}
