//! Decoder for encoded Protobuf messages using the descriptors

use std::fmt::{Display, Formatter};
use std::str::from_utf8;

use anyhow::anyhow;
use bytes::{Buf, BytesMut};
use itertools::Itertools;
use prost::encoding::{decode_key, decode_varint, encode_varint, WireType};
use prost_types::{DescriptorProto, EnumDescriptorProto, FieldDescriptorProto, FileDescriptorSet};
use prost_types::field_descriptor_proto::Type;
use tracing::{error, trace, warn};

use crate::utils::{as_hex, find_message_type_by_name, last_name};

/// Decoded Protobuf field
#[derive(Clone, Debug, PartialEq)]
pub struct ProtobufField {
  /// Field number
  pub field_num: u32,
  /// Wire type for the field
  pub wire_type: WireType,
  /// Field data
  pub data: ProtobufFieldData
}

impl ProtobufField {
  /// Create a copy of this field with the value replaced with the default
  pub fn default_field_value(&self, descriptor: &FieldDescriptorProto) -> ProtobufField {
    ProtobufField {
      field_num: self.field_num,
      wire_type: self.wire_type,
      data: self.data.default_field_value(descriptor)
    }
  }
}

impl Display for ProtobufField {
  fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
    write!(f, "{}:({:?}, {}) = {}", self.field_num, self.wire_type, self.data.type_name(), self.data)
  }
}

/// Decoded Protobuf field data
#[derive(Clone, Debug, PartialEq)]
pub enum ProtobufFieldData {
  /// String value
  String(String),
  /// Boolean value
  Boolean(bool),
  /// Unsigned 32 bit integer
  UInteger32(u32),
  /// Signed 32 bit integer
  Integer32(i32),
  /// Unsigned 64 bit integer
  UInteger64(u64),
  /// Signed 64 bit integer
  Integer64(i64),
  /// 32 bit floating point number
  Float(f32),
  /// 64 bit floating point number
  Double(f64),
  /// Array of bytes
  Bytes(Vec<u8>),
  /// Enum value
  Enum(i32, EnumDescriptorProto),
  /// Embedded message
  Message(Vec<u8>, DescriptorProto),
  /// For field data that does not match the descriptor
  Unknown(Vec<u8>)
}

impl ProtobufFieldData {
  /// Returns the type name of the field.
  pub fn type_name(&self) -> &'static str {
    match self {
      ProtobufFieldData::String(_) => "String",
      ProtobufFieldData::Boolean(_) => "Boolean",
      ProtobufFieldData::UInteger32(_) => "UInteger32",
      ProtobufFieldData::Integer32(_) => "Integer32",
      ProtobufFieldData::UInteger64(_) => "UInteger64",
      ProtobufFieldData::Integer64(_) => "Integer64",
      ProtobufFieldData::Float(_) => "Float",
      ProtobufFieldData::Double(_) => "Double",
      ProtobufFieldData::Bytes(_) => "Bytes",
      ProtobufFieldData::Enum(_, _) => "Enum",
      ProtobufFieldData::Message(_, _) => "Message",
      ProtobufFieldData::Unknown(_) => "Unknown"
    }
  }

  /// Converts the data for this value into a byte array
  pub fn as_bytes(&self) -> Vec<u8> {
    match self {
      ProtobufFieldData::String(s) => s.as_bytes().to_vec(),
      ProtobufFieldData::Boolean(b) => vec![ *b as u8 ],
      ProtobufFieldData::UInteger32(n) => n.to_le_bytes().to_vec(),
      ProtobufFieldData::Integer32(n) => n.to_le_bytes().to_vec(),
      ProtobufFieldData::UInteger64(n) => n.to_le_bytes().to_vec(),
      ProtobufFieldData::Integer64(n) => n.to_le_bytes().to_vec(),
      ProtobufFieldData::Float(n) => n.to_le_bytes().to_vec(),
      ProtobufFieldData::Double(n) => n.to_le_bytes().to_vec(),
      ProtobufFieldData::Bytes(b) => b.clone(),
      ProtobufFieldData::Enum(_, _) => self.to_string().as_bytes().to_vec(),
      ProtobufFieldData::Message(b, _) => b.clone(),
      ProtobufFieldData::Unknown(data) => data.clone()
    }
  }

  /// Return the default value for this field data
  pub fn default_field_value(&self, descriptor: &FieldDescriptorProto) -> ProtobufFieldData {
    match &descriptor.default_value {
      Some(s) => {
        // For numeric types, contains the original text representation of the value.
        // For booleans, "true" or "false".
        // For strings, contains the default text contents (not escaped in any way).
        // For bytes, contains the C escaped value.  All bytes >= 128 are escaped.
        match self {
          ProtobufFieldData::String(_) => ProtobufFieldData::String(s.clone()),
          ProtobufFieldData::Boolean(_) => ProtobufFieldData::Boolean(s == "true"),
          ProtobufFieldData::UInteger32(_) => ProtobufFieldData::UInteger32(s.parse().unwrap_or_default()),
          ProtobufFieldData::Integer32(_) => ProtobufFieldData::Integer32(s.parse().unwrap_or_default()),
          ProtobufFieldData::UInteger64(_) => ProtobufFieldData::UInteger64(s.parse().unwrap_or_default()),
          ProtobufFieldData::Integer64(_) => ProtobufFieldData::Integer64(s.parse().unwrap_or_default()),
          ProtobufFieldData::Float(_) => ProtobufFieldData::Float(s.parse().unwrap_or_default()),
          ProtobufFieldData::Double(_) => ProtobufFieldData::Double(s.parse().unwrap_or_default()),
          ProtobufFieldData::Bytes(_) => ProtobufFieldData::Bytes(s.as_bytes().to_vec()),
          ProtobufFieldData::Enum(_, descriptor) => ProtobufFieldData::Enum(s.parse().unwrap_or_default(), descriptor.clone()),
          ProtobufFieldData::Message(_, descriptor) => ProtobufFieldData::Message(Default::default(), descriptor.clone()),
          ProtobufFieldData::Unknown(_) => ProtobufFieldData::Unknown(Default::default())
        }
      }
      None => {
        // For strings, the default value is the empty string.
        // For bytes, the default value is empty bytes.
        // For bools, the default value is false.
        // For numeric types, the default value is zero.
        // For enums, the default value is the first defined enum value, which must be 0.
        // For message fields, the field is not set. Its exact value is language-dependent.
        match self {
          ProtobufFieldData::String(_) => ProtobufFieldData::String(Default::default()),
          ProtobufFieldData::Boolean(_) => ProtobufFieldData::Boolean(false),
          ProtobufFieldData::UInteger32(_) => ProtobufFieldData::UInteger32(0),
          ProtobufFieldData::Integer32(_) => ProtobufFieldData::Integer32(0),
          ProtobufFieldData::UInteger64(_) => ProtobufFieldData::UInteger64(0),
          ProtobufFieldData::Integer64(_) => ProtobufFieldData::Integer64(0),
          ProtobufFieldData::Float(_) => ProtobufFieldData::Float(0.0),
          ProtobufFieldData::Double(_) => ProtobufFieldData::Double(0.0),
          ProtobufFieldData::Bytes(_) => ProtobufFieldData::Bytes(Default::default()),
          ProtobufFieldData::Enum(_, descriptor) => ProtobufFieldData::Enum(0, descriptor.clone()),
          ProtobufFieldData::Message(_, descriptor) => ProtobufFieldData::Message(Default::default(), descriptor.clone()),
          ProtobufFieldData::Unknown(_) => ProtobufFieldData::Unknown(Default::default())
        }
      }
    }
  }
}

impl Display for ProtobufFieldData {
  fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
    match self {
      ProtobufFieldData::String(s) => write!(f, "\"{}\"", s),
      ProtobufFieldData::Boolean(b) => write!(f, "{}", b),
      ProtobufFieldData::UInteger32(n) => write!(f, "{}", n),
      ProtobufFieldData::Integer32(n) => write!(f, "{}", n),
      ProtobufFieldData::UInteger64(n) => write!(f, "{}", n),
      ProtobufFieldData::Integer64(n) => write!(f, "{}", n),
      ProtobufFieldData::Float(n) => write!(f, "{}", n),
      ProtobufFieldData::Double(n) => write!(f, "{}", n),
      ProtobufFieldData::Bytes(b) => if b.len() <= 16 {
        write!(f, "{}", as_hex(b.as_slice()))
      } else {
        write!(f, "{}... ({} bytes)", as_hex(&b[0..16]), b.len())
      },
      ProtobufFieldData::Enum(n, descriptor) => {
        let enum_value_name = descriptor.value.iter()
          .find(|v| v.number.is_some() && v.number.as_ref().unwrap() == n)
          .map(|v| v.name.clone().unwrap_or_default()).unwrap_or_else(|| "unknown".to_string());
        write!(f, "{}", enum_value_name)
      },
      ProtobufFieldData::Message(_, descriptor) => {
        write!(f, "{}", descriptor.name.clone().unwrap_or_else(|| "unknown".to_string()))
      }
      ProtobufFieldData::Unknown(b) => if b.len() <= 16 {
        write!(f, "{}", as_hex(b.as_slice()))
      } else {
        write!(f, "{}... ({} bytes)", as_hex(&b[0..16]), b.len())
      }
    }
  }
}

/// Decodes the Protobuf message using the descriptors
#[tracing::instrument(ret, skip_all)]
pub fn decode_message<B>(
  buffer: &mut B,
  descriptor: &DescriptorProto,
  descriptors: &FileDescriptorSet
) -> anyhow::Result<Vec<ProtobufField>>
  where B: Buf {
  let mut fields = vec![];

  while buffer.has_remaining() {
    let (field_num, wire_type) = decode_key(buffer)?;
    trace!(field_num, ?wire_type, "read field header, bytes remaining = {}", buffer.remaining());

    match find_field_descriptor(field_num as i32, descriptor) {
      Ok(field_descriptor) => {
        let data = match wire_type {
          WireType::Varint => {
            let varint = decode_varint(buffer)?;
            let t: Type = field_descriptor.r#type();
            match t {
              Type::Int64 => ProtobufFieldData::Integer64(varint as i64),
              Type::Uint64 => ProtobufFieldData::UInteger64(varint),
              Type::Int32 => ProtobufFieldData::Integer32(varint as i32),
              Type::Bool => ProtobufFieldData::Boolean(varint > 0),
              Type::Uint32 => ProtobufFieldData::UInteger32(varint as u32),
              Type::Enum => {
                let enum_proto = descriptor.enum_type.iter()
                  .find(|enum_type| enum_type.name.clone().unwrap_or_default() == last_name(field_descriptor.type_name.clone().unwrap_or_default().as_str()))
                  .ok_or_else(|| anyhow!("Did not find the enum {:?} for the field {} in the Protobuf descriptor", field_descriptor.type_name, field_num))?;
                ProtobufFieldData::Enum(varint as i32, enum_proto.clone())
              },
              Type::Sint32 => {
                let value = varint as u32;
                ProtobufFieldData::Integer32(((value >> 1) as i32) ^ (-((value & 1) as i32)))
              },
              Type::Sint64 => ProtobufFieldData::Integer64(((varint >> 1) as i64) ^ (-((varint & 1) as i64))),
              _ => {
                error!("Was expecting {:?} but received an unknown varint type", t);
                ProtobufFieldData::Unknown(varint.to_le_bytes().to_vec())
              }
            }
          }
          WireType::SixtyFourBit => {
            let t: Type = field_descriptor.r#type();
            match t {
              Type::Double => ProtobufFieldData::Double(buffer.get_f64_le()),
              Type::Fixed64 => ProtobufFieldData::UInteger64(buffer.get_u64_le()),
              Type::Sfixed64 => ProtobufFieldData::Integer64(buffer.get_i64_le()),
              _ => {
                error!("Was expecting {:?} but received an unknown 64 bit type", t);
                let value = buffer.get_u64_le();
                ProtobufFieldData::Unknown(value.to_le_bytes().to_vec())
              }
            }
          }
          WireType::LengthDelimited => {
            let data_length = decode_varint(buffer)?;
            let data_buffer = if buffer.remaining() >= data_length as usize {
              buffer.copy_to_bytes(data_length as usize)
            } else {
              return Err(anyhow!("Insufficient data remaining ({} bytes) to read {} bytes for field {}", buffer.remaining(), data_length, field_num));
            };
            let t: Type = field_descriptor.r#type();
            match t {
              Type::String => ProtobufFieldData::String(from_utf8(&data_buffer)?.to_string()),
              Type::Message => {
                let type_name = field_descriptor.type_name.as_ref().map(|v| last_name(v.as_str()).to_string());
                let message_proto = descriptor.nested_type.iter()
                  .find(|message_descriptor| message_descriptor.name == type_name)
                  .cloned()
                  .or_else(|| find_message_type_by_name(&type_name.unwrap_or_default(), descriptors).ok())
                  .ok_or_else(|| anyhow!("Did not find the embedded message {:?} for the field {} in the Protobuf descriptor", field_descriptor.type_name, field_num))?;
                ProtobufFieldData::Message(data_buffer.to_vec(), message_proto)
              }
              Type::Bytes => ProtobufFieldData::Bytes(data_buffer.to_vec()),
              _ => {
                error!("Was expecting {:?} but received an unknown length-delimited type", t);
                let mut buf = BytesMut::with_capacity((data_length + 8) as usize);
                encode_varint(data_length, &mut buf);
                buf.extend_from_slice(&*data_buffer);
                ProtobufFieldData::Unknown(buf.freeze().to_vec())
              }
            }
          }
          WireType::ThirtyTwoBit => {
            let t: Type = field_descriptor.r#type();
            match t {
              Type::Float => ProtobufFieldData::Float(buffer.get_f32_le()),
              Type::Fixed32 => ProtobufFieldData::UInteger32(buffer.get_u32_le()),
              Type::Sfixed32 => ProtobufFieldData::Integer32(buffer.get_i32_le()),
              _ => {
                error!("Was expecting {:?} but received an unknown fixed 32 bit type", t);
                let value = buffer.get_u32_le();
                ProtobufFieldData::Unknown(value.to_le_bytes().to_vec())
              }
            }
          }
          _ => return Err(anyhow!("Messages with {:?} wire type fields are not supported", wire_type))
        };

        trace!(field_num, ?wire_type, ?data, "read field, bytes remaining = {}", buffer.remaining());
        fields.push(ProtobufField {
          field_num,
          wire_type,
          data
        });
      }
      Err(err) => {
        warn!("Was not able to decode field: {}", err);
        let data = match wire_type {
          WireType::Varint => decode_varint(buffer)?.to_le_bytes().to_vec(),
          WireType::SixtyFourBit => buffer.get_u64().to_le_bytes().to_vec(),
          WireType::LengthDelimited => {
            let data_length = decode_varint(buffer)?;
            let mut buf = BytesMut::with_capacity((data_length + 8) as usize);
            encode_varint(data_length, &mut buf);
            buf.extend_from_slice(&*buffer.copy_to_bytes(data_length as usize));
            buf.freeze().to_vec()
          }
          WireType::ThirtyTwoBit => buffer.get_u32().to_le_bytes().to_vec(),
          _ => return Err(anyhow!("Messages with {:?} wire type fields are not supported", wire_type))
        };
        fields.push(ProtobufField {
          field_num,
          wire_type,
          data: ProtobufFieldData::Unknown(data)
        });
      }
    }
  }

  Ok(fields.iter().sorted_by(|a, b| Ord::cmp(&a.field_num, &b.field_num)).cloned().collect())
}

fn find_field_descriptor(field_num: i32, descriptor: &DescriptorProto) -> anyhow::Result<FieldDescriptorProto> {
  descriptor.field.iter().find(|field| {
    if let Some(num)  = field.number {
      num == field_num
    } else {
      false
    }
  })
    .cloned()
    .ok_or_else(|| anyhow!("Did not find a field with number {} in the descriptor", field_num))
}

#[cfg(test)]
mod tests {
  use bytes::{BufMut, Bytes, BytesMut};
  use expectest::prelude::*;
  use pact_plugin_driver::proto::InitPluginRequest;
  use prost::encoding::WireType;
  use prost::Message;
  use prost_types::{DescriptorProto, EnumDescriptorProto, EnumValueDescriptorProto, FileDescriptorSet};

  use crate::{
    bool_field_descriptor, 
    message_field_descriptor, 
    string_field_descriptor,
    enum_field_descriptor,
    u32_field_descriptor,
    i32_field_descriptor,
    u64_field_descriptor,
    i64_field_descriptor,
    f32_field_descriptor,
    f64_field_descriptor,
    bytes_field_descriptor
  };
  use crate::message_decoder::{decode_message, ProtobufFieldData};

  const FIELD_1_MESSAGE: [u8; 2] = [8, 1];
  const FIELD_2_MESSAGE: [u8; 2] = [16, 55];
  const FIELD_5_MESSAGE: [u8; 3] = [0b101000, 0b10110011, 0b101011];

  #[test]
  fn decode_boolean() {
    let mut buffer = Bytes::from_static(&FIELD_1_MESSAGE);
    let descriptor = DescriptorProto {
      name: Some("TestMessage".to_string()),
      field: vec![ bool_field_descriptor!("bool_field", 1) ],
      extension: vec![],
      nested_type: vec![],
      enum_type: vec![],
      extension_range: vec![],
      oneof_decl: vec![],
      options: None,
      reserved_range: vec![],
      reserved_name: vec![]
    };

    let result = decode_message(&mut buffer, &descriptor, &FileDescriptorSet{ file: vec![] }).unwrap();
    expect!(result.len()).to(be_equal_to(1));

    let field_result = result.first().unwrap();

    expect!(field_result.field_num).to(be_equal_to(1));
    expect!(field_result.wire_type).to(be_equal_to(WireType::Varint));
    expect!(&field_result.data).to(be_equal_to(&ProtobufFieldData::Boolean(true)));
  }

  #[test]
  fn decode_int32() {
    let mut buffer = Bytes::from_static(&FIELD_2_MESSAGE);
    let descriptor = DescriptorProto {
      name: Some("TestMessage".to_string()),
      field: vec![
        prost_types::FieldDescriptorProto {
          name: Some("field_1".to_string()),
          number: Some(2),
          label: Some(prost_types::field_descriptor_proto::Label::Optional as i32),
          r#type: Some(prost_types::field_descriptor_proto::Type::Int32 as i32),
          type_name: Some("Int32".to_string()),
          extendee: None,
          default_value: None,
          oneof_index: None,
          json_name: None,
          options: None,
          proto3_optional: None
        }
      ],
      extension: vec![],
      nested_type: vec![],
      enum_type: vec![],
      extension_range: vec![],
      oneof_decl: vec![],
      options: None,
      reserved_range: vec![],
      reserved_name: vec![]
    };

    let result = decode_message(&mut buffer, &descriptor, &FileDescriptorSet{ file: vec![] }).unwrap();
    expect!(result.len()).to(be_equal_to(1));

    let field_result = result.first().unwrap();

    expect!(field_result.field_num).to(be_equal_to(2));
    expect!(field_result.wire_type).to(be_equal_to(WireType::Varint));
    expect!(&field_result.data).to(be_equal_to(&ProtobufFieldData::Integer32(55)));
  }

  #[test]
  fn decode_uint64() {
    let mut buffer = Bytes::from_static(&FIELD_5_MESSAGE);
    let descriptor = DescriptorProto {
      name: Some("TestMessage".to_string()),
      field: vec![
        prost_types::FieldDescriptorProto {
          name: Some("field_1".to_string()),
          number: Some(5),
          label: Some(prost_types::field_descriptor_proto::Label::Optional as i32),
          r#type: Some(prost_types::field_descriptor_proto::Type::Uint64 as i32),
          type_name: Some("Uint64".to_string()),
          extendee: None,
          default_value: None,
          oneof_index: None,
          json_name: None,
          options: None,
          proto3_optional: None
        }
      ],
      extension: vec![],
      nested_type: vec![],
      enum_type: vec![],
      extension_range: vec![],
      oneof_decl: vec![],
      options: None,
      reserved_range: vec![],
      reserved_name: vec![]
    };

    let result = decode_message(&mut buffer, &descriptor, &FileDescriptorSet{ file: vec![] }).unwrap();
    expect!(result.len()).to(be_equal_to(1));

    let field_result = result.first().unwrap();

    expect!(field_result.field_num).to(be_equal_to(5));
    expect!(field_result.wire_type).to(be_equal_to(WireType::Varint));
    expect!(&field_result.data).to(be_equal_to(&ProtobufFieldData::UInteger64(5555)));
  }

  #[test]
  fn decode_enum() {
    let mut buffer = Bytes::from_static(&FIELD_2_MESSAGE);
    let enum_descriptor = EnumDescriptorProto {
      name: Some("ContentTypeHint".to_string()),
      value: vec![
        EnumValueDescriptorProto {
          name: Some("DEFAULT".to_string()),
          number: Some(0),
          options: None
        },
        EnumValueDescriptorProto {
          name: Some("TEXT".to_string()),
          number: Some(55),
          options: None
        },
        EnumValueDescriptorProto {
          name: Some("BINARY".to_string()),
          number: Some(66),
          options: None
        }
      ],
      options: None,
      reserved_range: vec![],
      reserved_name: vec![]
    };
    let descriptor = DescriptorProto {
      name: Some("TestMessage".to_string()),
      field: vec![
        prost_types::FieldDescriptorProto {
          name: Some("field_1".to_string()),
          number: Some(2),
          label: Some(prost_types::field_descriptor_proto::Label::Optional as i32),
          r#type: Some(prost_types::field_descriptor_proto::Type::Enum as i32),
          type_name: Some("ContentTypeHint".to_string()),
          extendee: None,
          default_value: None,
          oneof_index: None,
          json_name: None,
          options: None,
          proto3_optional: None
        }
      ],
      extension: vec![],
      nested_type: vec![],
      enum_type: vec![ enum_descriptor.clone() ],
      extension_range: vec![],
      oneof_decl: vec![],
      options: None,
      reserved_range: vec![],
      reserved_name: vec![]
    };

    let result = decode_message(&mut buffer, &descriptor, &FileDescriptorSet{ file: vec![] }).unwrap();
    expect!(result.len()).to(be_equal_to(1));

    let field_result = result.first().unwrap();

    expect!(field_result.field_num).to(be_equal_to(2));
    expect!(field_result.wire_type).to(be_equal_to(WireType::Varint));
    expect!(&field_result.data).to(be_equal_to(&ProtobufFieldData::Enum(55, enum_descriptor)));
  }

  #[test]
  fn decode_f32() {
    let f_value: f32 = 12.34;
    let mut buffer = BytesMut::new();
    buffer.put_u8(21);
    buffer.put_f32_le(f_value);

    let descriptor = DescriptorProto {
      name: Some("TestMessage".to_string()),
      field: vec![
        prost_types::FieldDescriptorProto {
          name: Some("field_1".to_string()),
          number: Some(2),
          label: Some(prost_types::field_descriptor_proto::Label::Optional as i32),
          r#type: Some(prost_types::field_descriptor_proto::Type::Float as i32),
          type_name: Some("Float".to_string()),
          extendee: None,
          default_value: None,
          oneof_index: None,
          json_name: None,
          options: None,
          proto3_optional: None
        }
      ],
      extension: vec![],
      nested_type: vec![],
      enum_type: vec![],
      extension_range: vec![],
      oneof_decl: vec![],
      options: None,
      reserved_range: vec![],
      reserved_name: vec![]
    };

    let result = decode_message(&mut buffer.freeze(), &descriptor, &FileDescriptorSet{ file: vec![] }).unwrap();
    expect!(result.len()).to(be_equal_to(1));

    let field_result = result.first().unwrap();

    expect!(field_result.field_num).to(be_equal_to(2));
    expect!(field_result.wire_type).to(be_equal_to(WireType::ThirtyTwoBit));
    expect!(&field_result.data).to(be_equal_to(&ProtobufFieldData::Float(12.34)));
  }

  #[test]
  fn decode_f64() {
    let f_value: f64 = 12.34;
    let mut buffer = BytesMut::new();
    buffer.put_u8(17);
    buffer.put_f64_le(f_value);

    let descriptor = DescriptorProto {
      name: Some("TestMessage".to_string()),
      field: vec![
        prost_types::FieldDescriptorProto {
          name: Some("field_1".to_string()),
          number: Some(2),
          label: Some(prost_types::field_descriptor_proto::Label::Optional as i32),
          r#type: Some(prost_types::field_descriptor_proto::Type::Double as i32),
          type_name: Some("Double".to_string()),
          extendee: None,
          default_value: None,
          oneof_index: None,
          json_name: None,
          options: None,
          proto3_optional: None
        }
      ],
      extension: vec![],
      nested_type: vec![],
      enum_type: vec![],
      extension_range: vec![],
      oneof_decl: vec![],
      options: None,
      reserved_range: vec![],
      reserved_name: vec![]
    };

    let result = decode_message(&mut buffer, &descriptor, &FileDescriptorSet{ file: vec![] }).unwrap();
    expect!(result.len()).to(be_equal_to(1));

    let field_result = result.first().unwrap();

    expect!(field_result.field_num).to(be_equal_to(2));
    expect!(field_result.wire_type).to(be_equal_to(WireType::SixtyFourBit));
    expect!(&field_result.data).to(be_equal_to(&ProtobufFieldData::Double(12.34)));
  }

  #[test]
  fn decode_string() {
    let str_data = "this is a string!";
    let mut buffer = BytesMut::new();
    buffer.put_u8(10);
    buffer.put_u8(str_data.len() as u8);
    buffer.put_slice(str_data.as_bytes());

    let descriptor = DescriptorProto {
      name: Some("TestMessage".to_string()),
      field: vec![
        string_field_descriptor!("type", 1)
      ],
      extension: vec![],
      nested_type: vec![],
      enum_type: vec![],
      extension_range: vec![],
      oneof_decl: vec![],
      options: None,
      reserved_range: vec![],
      reserved_name: vec![]
    };

    let result = decode_message(&mut buffer, &descriptor, &FileDescriptorSet{ file: vec![] }).unwrap();
    expect!(result.len()).to(be_equal_to(1));

    let field_result = result.first().unwrap();

    expect!(field_result.field_num).to(be_equal_to(1));
    expect!(field_result.wire_type).to(be_equal_to(WireType::LengthDelimited));
    expect!(&field_result.data).to(be_equal_to(&ProtobufFieldData::String(str_data.to_string())));
  }

  #[test]
  fn decode_message_test() {
    let message = InitPluginRequest {
      implementation: "test".to_string(),
      version: "1.2.3.4".to_string()
    };

    let field1 = string_field_descriptor!("implementation", 1);
    let field2 = string_field_descriptor!("version", 2);
    let message_descriptor = DescriptorProto {
      name: Some("InitPluginRequest".to_string()),
      field: vec![
        field1.clone(),
        field2.clone()
      ],
      extension: vec![],
      nested_type: vec![],
      enum_type: vec![],
      extension_range: vec![],
      oneof_decl: vec![],
      options: None,
      reserved_range: vec![],
      reserved_name: vec![]
    };
    let encoded = message.encode_to_vec();

    let mut buffer = BytesMut::new();
    buffer.put_u8(10);
    buffer.put_u8(encoded.len() as u8);
    buffer.put_slice(&encoded);

    let descriptor = DescriptorProto {
      name: Some("TestMessage".to_string()),
      field: vec![
        message_field_descriptor!("message", 1, "InitPluginRequest")
      ],
      extension: vec![],
      nested_type: vec![
        message_descriptor.clone()
      ],
      enum_type: vec![],
      extension_range: vec![],
      oneof_decl: vec![],
      options: None,
      reserved_range: vec![],
      reserved_name: vec![]
    };

    let result = decode_message(&mut buffer, &descriptor, &FileDescriptorSet{ file: vec![] }).unwrap();
    expect!(result.len()).to(be_equal_to(1));

    let field_result = result.first().unwrap();

    expect!(field_result.field_num).to(be_equal_to(1));
    expect!(field_result.wire_type).to(be_equal_to(WireType::LengthDelimited));
    expect!(&field_result.data).to(be_equal_to(&ProtobufFieldData::Message(encoded, message_descriptor)));
  }

  #[test]
  fn decode_message_with_unknown_field() {
    let message = InitPluginRequest {
      implementation: "test".to_string(),
      version: "1.2.3.4".to_string()
    };

    let field1 = string_field_descriptor!("implementation", 1);
    let message_descriptor = DescriptorProto {
      name: Some("InitPluginRequest".to_string()),
      field: vec![
        field1.clone()
      ],
      extension: vec![],
      nested_type: vec![],
      enum_type: vec![],
      extension_range: vec![],
      oneof_decl: vec![],
      options: None,
      reserved_range: vec![],
      reserved_name: vec![]
    };

    let mut buffer = BytesMut::from(message.encode_to_vec().as_slice());
    let result = decode_message(&mut buffer, &message_descriptor, &FileDescriptorSet{ file: vec![] }).unwrap();
    expect!(result.len()).to(be_equal_to(2));

    let field_result = result.first().unwrap();
    expect!(field_result.field_num).to(be_equal_to(1));
    expect!(field_result.wire_type).to(be_equal_to(WireType::LengthDelimited));
    expect!(&field_result.data).to(be_equal_to(&ProtobufFieldData::String("test".to_string())));

    let field_result = result.get(1).unwrap();
    expect!(field_result.field_num).to(be_equal_to(2));
    expect!(field_result.wire_type).to(be_equal_to(WireType::LengthDelimited));
    expect!(field_result.data.type_name()).to(be_equal_to("Unknown"));
  }

  #[test]
  fn default_field_value_test_boolean() {
    let descriptor = bool_field_descriptor!("bool_field", 1);
    expect!(ProtobufFieldData::Boolean(true).default_field_value(&descriptor)).to(be_equal_to(ProtobufFieldData::Boolean(false)));

    let descriptor = prost_types::FieldDescriptorProto {
      default_value: Some("true".to_string()),
      .. bool_field_descriptor!("bool_field", 1)
    };
    expect!(ProtobufFieldData::Boolean(true).default_field_value(&descriptor)).to(be_equal_to(ProtobufFieldData::Boolean(true)));

    let descriptor = prost_types::FieldDescriptorProto {
      default_value: Some("false".to_string()),
      .. bool_field_descriptor!("bool_field", 1)
    };
    expect!(ProtobufFieldData::Boolean(true).default_field_value(&descriptor)).to(be_equal_to(ProtobufFieldData::Boolean(false)));
  }

  #[test]
  fn default_field_value_test_string() {
    let descriptor = string_field_descriptor!("field", 1);
    expect!(ProtobufFieldData::String("true".to_string()).default_field_value(&descriptor)).to(be_equal_to(ProtobufFieldData::String("".to_string())));

    let descriptor = prost_types::FieldDescriptorProto {
      default_value: Some("true".to_string()),
      .. string_field_descriptor!("field", 1)
    };
    expect!(ProtobufFieldData::String("other".to_string()).default_field_value(&descriptor)).to(be_equal_to(ProtobufFieldData::String("true".to_string())));
  }

  #[test]
  fn default_field_value_test_u32() {
    let descriptor = u32_field_descriptor!("field", 1);
    expect!(ProtobufFieldData::UInteger32(123).default_field_value(&descriptor)).to(be_equal_to(ProtobufFieldData::UInteger32(0)));

    let descriptor = prost_types::FieldDescriptorProto {
      default_value: Some("100".to_string()),
      .. u32_field_descriptor!("field", 1)
    };
    expect!(ProtobufFieldData::UInteger32(123).default_field_value(&descriptor)).to(be_equal_to(ProtobufFieldData::UInteger32(100)));

    let descriptor = prost_types::FieldDescriptorProto {
      default_value: Some("sdsd".to_string()),
      .. u32_field_descriptor!("field", 1)
    };
    expect!(ProtobufFieldData::UInteger32(123).default_field_value(&descriptor)).to(be_equal_to(ProtobufFieldData::UInteger32(0)));
  }

  #[test]
  fn default_field_value_test_i32() {
    let descriptor = i32_field_descriptor!("field", 1);
    expect!(ProtobufFieldData::Integer32(123).default_field_value(&descriptor)).to(be_equal_to(ProtobufFieldData::Integer32(0)));

    let descriptor = prost_types::FieldDescriptorProto {
      default_value: Some("100".to_string()),
      .. i32_field_descriptor!("field", 1)
    };
    expect!(ProtobufFieldData::Integer32(123).default_field_value(&descriptor)).to(be_equal_to(ProtobufFieldData::Integer32(100)));

    let descriptor = prost_types::FieldDescriptorProto {
      default_value: Some("sdsd".to_string()),
      .. i32_field_descriptor!("field", 1)
    };
    expect!(ProtobufFieldData::Integer32(123).default_field_value(&descriptor)).to(be_equal_to(ProtobufFieldData::Integer32(0)));
  }

  #[test]
  fn default_field_value_test_u64() {
    let descriptor = u64_field_descriptor!("field", 1);
    expect!(ProtobufFieldData::UInteger64(123).default_field_value(&descriptor)).to(be_equal_to(ProtobufFieldData::UInteger64(0)));

    let descriptor = prost_types::FieldDescriptorProto {
      default_value: Some("100".to_string()),
      .. u64_field_descriptor!("field", 1)
    };
    expect!(ProtobufFieldData::UInteger64(123).default_field_value(&descriptor)).to(be_equal_to(ProtobufFieldData::UInteger64(100)));

    let descriptor = prost_types::FieldDescriptorProto {
      default_value: Some("sdsd".to_string()),
      .. u64_field_descriptor!("field", 1)
    };
    expect!(ProtobufFieldData::UInteger64(123).default_field_value(&descriptor)).to(be_equal_to(ProtobufFieldData::UInteger64(0)));
  }

  #[test]
  fn default_field_value_test_i64() {
    let descriptor = i64_field_descriptor!("field", 1);
    expect!(ProtobufFieldData::Integer64(123).default_field_value(&descriptor)).to(be_equal_to(ProtobufFieldData::Integer64(0)));

    let descriptor = prost_types::FieldDescriptorProto {
      default_value: Some("100".to_string()),
      .. i64_field_descriptor!("field", 1)
    };
    expect!(ProtobufFieldData::Integer64(123).default_field_value(&descriptor)).to(be_equal_to(ProtobufFieldData::Integer64(100)));

    let descriptor = prost_types::FieldDescriptorProto {
      default_value: Some("sdsd".to_string()),
      .. i64_field_descriptor!("field", 1)
    };
    expect!(ProtobufFieldData::Integer64(123).default_field_value(&descriptor)).to(be_equal_to(ProtobufFieldData::Integer64(0)));
  }

  #[test]
  fn default_field_value_test_f32() {
    let descriptor = f32_field_descriptor!("field", 1);
    expect!(ProtobufFieldData::Float(123.0).default_field_value(&descriptor)).to(be_equal_to(ProtobufFieldData::Float(0.0)));

    let descriptor = prost_types::FieldDescriptorProto {
      default_value: Some("100".to_string()),
      .. f32_field_descriptor!("field", 1)
    };
    expect!(ProtobufFieldData::Float(123.0).default_field_value(&descriptor)).to(be_equal_to(ProtobufFieldData::Float(100.0)));

    let descriptor = prost_types::FieldDescriptorProto {
      default_value: Some("sdsd".to_string()),
      .. f32_field_descriptor!("field", 1)
    };
    expect!(ProtobufFieldData::Float(123.0).default_field_value(&descriptor)).to(be_equal_to(ProtobufFieldData::Float(0.0)));
  }

  #[test]
  fn default_field_value_test_f64() {
    let descriptor = f64_field_descriptor!("field", 1);
    expect!(ProtobufFieldData::Double(123.0).default_field_value(&descriptor)).to(be_equal_to(ProtobufFieldData::Double(0.0)));

    let descriptor = prost_types::FieldDescriptorProto {
      default_value: Some("100".to_string()),
      .. f64_field_descriptor!("field", 1)
    };
    expect!(ProtobufFieldData::Double(123.0).default_field_value(&descriptor)).to(be_equal_to(ProtobufFieldData::Double(100.0)));

    let descriptor = prost_types::FieldDescriptorProto {
      default_value: Some("sdsd".to_string()),
      .. f64_field_descriptor!("field", 1)
    };
    expect!(ProtobufFieldData::Double(123.0).default_field_value(&descriptor)).to(be_equal_to(ProtobufFieldData::Double(0.0)));
  }

  #[test]
  fn default_field_value_test_enum() {
    let enum_descriptor = prost_types::EnumDescriptorProto {
      name: Some("EnumValue".to_string()),
      value: vec![
        prost_types::EnumValueDescriptorProto {
          name: Some("OPT1".to_string()),
          number: Some(0),
          options: None
        },
        prost_types::EnumValueDescriptorProto {
          name: Some("OPT2".to_string()),
          number: Some(1),
          options: None
        },
        prost_types::EnumValueDescriptorProto {
          name: Some("OPT3".to_string()),
          number: Some(2),
          options: None
        }
      ],
      options: None,
      reserved_range: vec![],
      reserved_name: vec![]
    };
    let descriptor = enum_field_descriptor!("field", 1, "OPT1");
    expect!(ProtobufFieldData::Enum(2, enum_descriptor.clone()).default_field_value(&descriptor)).to(be_equal_to(ProtobufFieldData::Enum(0, enum_descriptor.clone())));

    let descriptor = prost_types::FieldDescriptorProto {
      default_value: Some("1".to_string()),
      .. enum_field_descriptor!("field", 1, "OPT2")
    };
    expect!(ProtobufFieldData::Enum(2, enum_descriptor.clone()).default_field_value(&descriptor)).to(be_equal_to(ProtobufFieldData::Enum(1, enum_descriptor.clone())));

    let descriptor = prost_types::FieldDescriptorProto {
      default_value: Some("sdsd".to_string()),
      .. enum_field_descriptor!("field", 1, "OPT2")
    };
    expect!(ProtobufFieldData::Enum(2, enum_descriptor.clone()).default_field_value(&descriptor)).to(be_equal_to(ProtobufFieldData::Enum(0, enum_descriptor.clone())));
  }

  #[test]
  fn default_field_value_test_bytes() {
    let descriptor = bytes_field_descriptor!("field", 1);
    expect!(ProtobufFieldData::Bytes(vec![1, 2, 3, 4]).default_field_value(&descriptor)).to(be_equal_to(ProtobufFieldData::Bytes(vec![])));

    let descriptor = prost_types::FieldDescriptorProto {
      default_value: Some("true".to_string()),
      .. bytes_field_descriptor!("field", 1)
    };
    expect!(ProtobufFieldData::Bytes(vec![1, 2, 3, 4]).default_field_value(&descriptor)).to(be_equal_to(ProtobufFieldData::Bytes(vec![116, 114, 117, 101])));
  }

  #[test]
  fn default_field_value_test_message() {
    let field1 = string_field_descriptor!("implementation", 1);
    let field2 = string_field_descriptor!("version", 2);
    let message_descriptor = DescriptorProto {
      name: Some("InitPluginRequest".to_string()),
      field: vec![
        field1.clone(),
        field2.clone()
      ],
      extension: vec![],
      nested_type: vec![],
      enum_type: vec![],
      extension_range: vec![],
      oneof_decl: vec![],
      options: None,
      reserved_range: vec![],
      reserved_name: vec![]
    };
    let descriptor = message_field_descriptor!("field", 1, "InitPluginRequest");
    expect!(ProtobufFieldData::Message(vec![1, 2, 3, 4], message_descriptor.clone()).default_field_value(&descriptor)).to(be_equal_to(ProtobufFieldData::Message(vec![], message_descriptor.clone())));

    let descriptor = prost_types::FieldDescriptorProto {
      default_value: Some("true".to_string()),
      .. message_field_descriptor!("field", 1, "InitPluginRequest")
    };
    expect!(ProtobufFieldData::Message(vec![1, 2, 3, 4], message_descriptor.clone()).default_field_value(&descriptor)).to(be_equal_to(ProtobufFieldData::Message(vec![], message_descriptor.clone())));
  }

  #[test]
  fn default_field_value_test_unknown() {
    let descriptor = bytes_field_descriptor!("field", 1);
    expect!(ProtobufFieldData::Unknown(vec![1, 2, 3, 4]).default_field_value(&descriptor)).to(be_equal_to(ProtobufFieldData::Unknown(vec![])));

    let descriptor = prost_types::FieldDescriptorProto {
      default_value: Some("true".to_string()),
      .. bytes_field_descriptor!("field", 1)
    };
    expect!(ProtobufFieldData::Unknown(vec![1, 2, 3, 4]).default_field_value(&descriptor)).to(be_equal_to(ProtobufFieldData::Unknown(vec![])));
  }
}
