use std::slice::Iter;

use crate::router::Ack;

use super::*;
use bytes::{Buf, BufMut, Bytes, BytesMut};

mod connack;
mod connect;
mod disconnect;
mod ping;
mod puback;
mod pubcomp;
mod publish;
mod pubrec;
mod pubrel;
mod suback;
mod subscribe;
mod unsuback;
mod unsubscribe;

/// MQTT packet type
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PacketType {
    Connect = 1,
    ConnAck,
    Publish,
    PubAck,
    PubRec,
    PubRel,
    PubComp,
    Subscribe,
    SubAck,
    Unsubscribe,
    UnsubAck,
    PingReq,
    PingResp,
    Disconnect,
}

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PropertyType {
    PayloadFormatIndicator = 1,
    MessageExpiryInterval = 2,
    ContentType = 3,
    ResponseTopic = 8,
    CorrelationData = 9,
    SubscriptionIdentifier = 11,
    SessionExpiryInterval = 17,
    AssignedClientIdentifier = 18,
    ServerKeepAlive = 19,
    AuthenticationMethod = 21,
    AuthenticationData = 22,
    RequestProblemInformation = 23,
    WillDelayInterval = 24,
    RequestResponseInformation = 25,
    ResponseInformation = 26,
    ServerReference = 28,
    ReasonString = 31,
    ReceiveMaximum = 33,
    TopicAliasMaximum = 34,
    TopicAlias = 35,
    MaximumQos = 36,
    RetainAvailable = 37,
    UserProperty = 38,
    MaximumPacketSize = 39,
    WildcardSubscriptionAvailable = 40,
    SubscriptionIdentifierAvailable = 41,
    SharedSubscriptionAvailable = 42,
}

/// Packet type from a byte
///
/// ```ignore
///          7                          3                          0
///          +--------------------------+--------------------------+
/// byte 1   | MQTT Control Packet Type | Flags for each type      |
///          +--------------------------+--------------------------+
///          |         Remaining Bytes Len  (1/2/3/4 bytes)        |
///          +-----------------------------------------------------+
///
/// http://docs.oasis-open.org/mqtt/mqtt/v3.1.1/os/mqtt-v3.1.1-os.html#_Figure_2.2_-
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd)]
pub struct FixedHeader {
    /// First byte of the stream. Used to identify packet types and
    /// several flags
    byte1: u8,
    /// Length of fixed header. Byte 1 + (1..4) bytes. So fixed header
    /// len can vary from 2 bytes to 5 bytes
    /// 1..4 bytes are variable length encoded to represent remaining length
    fixed_header_len: usize,
    /// Remaining length of the packet. Doesn't include fixed header bytes
    /// Represents variable header + payload size
    remaining_len: usize,
}

impl FixedHeader {
    pub fn new(byte1: u8, remaining_len_len: usize, remaining_len: usize) -> FixedHeader {
        FixedHeader {
            byte1,
            fixed_header_len: remaining_len_len + 1,
            remaining_len,
        }
    }

    pub fn packet_type(&self) -> Result<PacketType, Error> {
        let num = self.byte1 >> 4;
        match num {
            1 => Ok(PacketType::Connect),
            2 => Ok(PacketType::ConnAck),
            3 => Ok(PacketType::Publish),
            4 => Ok(PacketType::PubAck),
            5 => Ok(PacketType::PubRec),
            6 => Ok(PacketType::PubRel),
            7 => Ok(PacketType::PubComp),
            8 => Ok(PacketType::Subscribe),
            9 => Ok(PacketType::SubAck),
            10 => Ok(PacketType::Unsubscribe),
            11 => Ok(PacketType::UnsubAck),
            12 => Ok(PacketType::PingReq),
            13 => Ok(PacketType::PingResp),
            14 => Ok(PacketType::Disconnect),
            _ => Err(Error::InvalidPacketType(num)),
        }
    }

    /// Returns the size of full packet (fixed header + variable header + payload)
    /// Fixed header is enough to get the size of a frame in the stream
    pub fn frame_length(&self) -> usize {
        self.fixed_header_len + self.remaining_len
    }
}

fn property(num: u8) -> Result<PropertyType, Error> {
    let property = match num {
        1 => PropertyType::PayloadFormatIndicator,
        2 => PropertyType::MessageExpiryInterval,
        3 => PropertyType::ContentType,
        8 => PropertyType::ResponseTopic,
        9 => PropertyType::CorrelationData,
        11 => PropertyType::SubscriptionIdentifier,
        17 => PropertyType::SessionExpiryInterval,
        18 => PropertyType::AssignedClientIdentifier,
        19 => PropertyType::ServerKeepAlive,
        21 => PropertyType::AuthenticationMethod,
        22 => PropertyType::AuthenticationData,
        23 => PropertyType::RequestProblemInformation,
        24 => PropertyType::WillDelayInterval,
        25 => PropertyType::RequestResponseInformation,
        26 => PropertyType::ResponseInformation,
        28 => PropertyType::ServerReference,
        31 => PropertyType::ReasonString,
        33 => PropertyType::ReceiveMaximum,
        34 => PropertyType::TopicAliasMaximum,
        35 => PropertyType::TopicAlias,
        36 => PropertyType::MaximumQos,
        37 => PropertyType::RetainAvailable,
        38 => PropertyType::UserProperty,
        39 => PropertyType::MaximumPacketSize,
        40 => PropertyType::WildcardSubscriptionAvailable,
        41 => PropertyType::SubscriptionIdentifierAvailable,
        42 => PropertyType::SharedSubscriptionAvailable,
        num => return Err(Error::InvalidPropertyType(num)),
    };

    Ok(property)
}

/// Checks if the stream has enough bytes to frame a packet and returns fixed header
/// only if a packet can be framed with existing bytes in the `stream`.
/// The passed stream doesn't modify parent stream's cursor. If this function
/// returned an error, next `check` on the same parent stream is forced start
/// with cursor at 0 again (Iter is owned. Only Iter's cursor is changed internally)
pub fn check(stream: Iter<u8>, max_packet_size: usize) -> Result<FixedHeader, Error> {
    // Create fixed header if there are enough bytes in the stream
    // to frame full packet
    let stream_len = stream.len();
    let fixed_header = parse_fixed_header(stream)?;

    // Don't let rogue connections attack with huge payloads.
    // Disconnect them before reading all that data
    if fixed_header.remaining_len > max_packet_size {
        return Err(Error::PayloadSizeLimitExceeded(fixed_header.remaining_len));
    }

    // If the current call fails due to insufficient bytes in the stream,
    // after calculating remaining length, we extend the stream
    let frame_length = fixed_header.frame_length();
    if stream_len < frame_length {
        return Err(Error::InsufficientBytes(frame_length - stream_len));
    }

    Ok(fixed_header)
}

/// Parses fixed header
fn parse_fixed_header(mut stream: Iter<u8>) -> Result<FixedHeader, Error> {
    // At least 2 bytes are necessary to frame a packet
    let stream_len = stream.len();
    if stream_len < 2 {
        return Err(Error::InsufficientBytes(2 - stream_len));
    }

    let byte1 = stream.next().unwrap();
    let (len_len, len) = length(stream)?;

    Ok(FixedHeader::new(*byte1, len_len, len))
}

/// Parses variable byte integer in the stream and returns the length
/// and number of bytes that make it. Used for remaining length calculation
/// as well as for calculating property lengths
fn length(stream: Iter<u8>) -> Result<(usize, usize), Error> {
    let mut len: usize = 0;
    let mut len_len = 0;
    let mut done = false;
    let mut shift = 0;

    // Use continuation bit at position 7 to continue reading next
    // byte to frame 'length'.
    // Stream 0b1xxx_xxxx 0b1yyy_yyyy 0b1zzz_zzzz 0b0www_wwww will
    // be framed as number 0bwww_wwww_zzz_zzzz_yyy_yyyy_xxx_xxxx
    for byte in stream {
        len_len += 1;
        let byte = *byte as usize;
        len += (byte & 0x7F) << shift;

        // stop when continue bit is 0
        done = (byte & 0x80) == 0;
        if done {
            break;
        }

        shift += 7;

        // Only a max of 4 bytes allowed for remaining length
        // more than 4 shifts (0, 7, 14, 21) implies bad length
        if shift > 21 {
            return Err(Error::MalformedRemainingLength);
        }
    }

    // Not enough bytes to frame remaining length. wait for
    // one more byte
    if !done {
        return Err(Error::InsufficientBytes(1));
    }

    Ok((len_len, len))
}

/// Reads a series of bytes with a length from a byte stream
fn read_mqtt_bytes(stream: &mut Bytes) -> Result<Bytes, Error> {
    let len = read_u16(stream)? as usize;

    // Prevent attacks with wrong remaining length. This method is used in
    // `packet.assembly()` with (enough) bytes to frame packet. Ensures that
    // reading variable len string or bytes doesn't cross promised boundary
    // with `read_fixed_header()`
    if len > stream.len() {
        return Err(Error::BoundaryCrossed(len));
    }

    Ok(stream.split_to(len))
}

/// Reads a string from bytes stream
fn read_mqtt_string(stream: &mut Bytes) -> Result<String, Error> {
    let s = read_mqtt_bytes(stream)?;
    match String::from_utf8(s.to_vec()) {
        Ok(v) => Ok(v),
        Err(_e) => Err(Error::TopicNotUtf8),
    }
}

/// Serializes bytes to stream (including length)
fn write_mqtt_bytes(stream: &mut BytesMut, bytes: &[u8]) {
    stream.put_u16(bytes.len() as u16);
    stream.extend_from_slice(bytes);
}

/// Serializes a string to stream
fn write_mqtt_string(stream: &mut BytesMut, string: &str) {
    write_mqtt_bytes(stream, string.as_bytes());
}

/// Writes remaining length to stream and returns number of bytes for remaining length
fn write_remaining_length(stream: &mut BytesMut, len: usize) -> Result<usize, Error> {
    if len > 268_435_455 {
        return Err(Error::PayloadTooLong);
    }

    let mut done = false;
    let mut x = len;
    let mut count = 0;

    while !done {
        let mut byte = (x % 128) as u8;
        x /= 128;
        if x > 0 {
            byte |= 128;
        }

        stream.put_u8(byte);
        count += 1;
        done = x == 0;
    }

    Ok(count)
}

/// Return number of remaining length bytes required for encoding length
fn len_len(len: usize) -> usize {
    if len >= 2_097_152 {
        4
    } else if len >= 16_384 {
        3
    } else if len >= 128 {
        2
    } else {
        1
    }
}

/// After collecting enough bytes to frame a packet (packet's frame())
/// , It's possible that content itself in the stream is wrong. Like expected
/// packet id or qos not being present. In cases where `read_mqtt_string` or
/// `read_mqtt_bytes` exhausted remaining length but packet framing expects to
/// parse qos next, these pre checks will prevent `bytes` crashes
fn read_u16(stream: &mut Bytes) -> Result<u16, Error> {
    if stream.len() < 2 {
        return Err(Error::MalformedPacket);
    }

    Ok(stream.get_u16())
}

fn read_u8(stream: &mut Bytes) -> Result<u8, Error> {
    if stream.is_empty() {
        return Err(Error::MalformedPacket);
    }

    Ok(stream.get_u8())
}

fn read_u32(stream: &mut Bytes) -> Result<u32, Error> {
    if stream.len() < 4 {
        return Err(Error::MalformedPacket);
    }

    Ok(stream.get_u32())
}

#[derive(Debug, Clone)]
pub struct V5;

impl Protocol for V5 {
    /// Reads a stream of bytes and extracts next MQTT packet out of it
    fn read_mut(&mut self, stream: &mut BytesMut, max_size: usize) -> Result<Packet, Error> {
        let fixed_header = check(stream.iter(), max_size)?;

        // Test with a stream with exactly the size to check border panics
        let packet = stream.split_to(fixed_header.frame_length());
        let packet_type = fixed_header.packet_type()?;

        if fixed_header.remaining_len == 0 {
            // no payload packets
            return match packet_type {
                PacketType::PingReq => Ok(Packet::PingReq(PingReq)),
                PacketType::PingResp => Ok(Packet::PingResp(PingResp)),
                PacketType::Disconnect => Ok(Packet::Disconnect),
                _ => Err(Error::PayloadRequired),
            };
        }

        let packet = packet.freeze();
        let packet = match packet_type {
            PacketType::Connect => {
                let (connect, properties, will, willproperties, login) =
                    connect::read(fixed_header, packet)?;
                Packet::Connect(connect, properties, will, willproperties, login)
            }
            PacketType::Publish => {
                let (publish, properties) = publish::read(fixed_header, packet)?;
                Packet::Publish(publish, properties)
            }
            PacketType::PubAck => {
                let (puback, properties) = puback::read(fixed_header, packet)?;
                Packet::PubAck(puback, properties)
            }
            PacketType::Subscribe => {
                let (subscribe, properties) = subscribe::read(fixed_header, packet)?;
                Packet::Subscribe(subscribe, properties)
            }
            PacketType::SubAck => {
                let (suback, properties) = suback::read(fixed_header, packet)?;
                Packet::SubAck(suback, properties)
            }
            PacketType::PingReq => Packet::PingReq(PingReq),
            PacketType::PingResp => Packet::PingResp(PingResp),
            PacketType::Disconnect => Packet::Disconnect,
            _ => unreachable!(),
        };

        Ok(packet)
    }

    fn write(&self, notification: Notification, write: &mut BytesMut) -> Result<bool, Error> {
        match notification {
            Notification::Forward(forward) => {
                publish::write(&forward.publish, &None, write)?;
            }
            Notification::ForwardWithProperties(forward, properties) => {
                publish::write(&forward.publish, &Some(properties), write)?;
            }
            Notification::DeviceAck(ack) => match ack {
                Ack::ConnAck(_, ack) => {
                    connack::write(&ack, &None, write)?;
                }
                Ack::PubAck(ack) => {
                    puback::write(&ack, &None, write)?;
                }
                Ack::PubAckWithProperties(ack, properties) => {
                    puback::write(&ack, &Some(properties), write)?;
                }
                Ack::SubAck(ack) => {
                    suback::write(&ack, &None, write)?;
                }
                Ack::SubAckWithProperties(ack, properties) => {
                    suback::write(&ack, &Some(properties), write)?;
                }
                Ack::PingResp(pingresp) => {
                    ping::pingresp::write(write)?;
                }
                Ack::PubRec(pubrec) => {
                    pubrec::write(&pubrec, &None, write)?;
                }
                Ack::PubRecWithProperties(pubrec, properties) => {
                    pubrec::write(&pubrec, &Some(properties), write)?;
                }
                Ack::PubRel(pubrel) => {
                    pubrel::write(&pubrel, &None, write)?;
                }
                Ack::PubRelWithProperties(pubrel, properties) => {
                    pubrel::write(&pubrel, &Some(properties), write)?;
                }
                Ack::PubComp(pubcomp) => {
                    pubcomp::write(&pubcomp, &None, write)?;
                }
                Ack::PubCompWithProperties(pubcomp, properties) => {
                    pubcomp::write(&pubcomp, &Some(properties), write)?;
                }
                _ => unimplemented!(),
            },
            Notification::Unschedule => return Ok(true),
            v => unreachable!("{:?}", v),
        }

        Ok(false)
    }
}