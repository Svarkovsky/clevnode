//! Typed codecs for the standard LXMF attachment extension fields.
//!
//! Unknown extension fields remain available through [`crate::Field`]; these
//! helpers only own the schemas defined for files, images and audio by LXMF.

use crate::{
    constants::{FIELD_AUDIO, FIELD_FILE_ATTACHMENTS, FIELD_IMAGE},
    msgpack, Field,
};
use alloc::{string::String, vec::Vec};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileAttachment {
    pub name: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageAttachment {
    pub format: String,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioAttachment {
    pub mode: u8,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MessageAttachments {
    pub files: Vec<FileAttachment>,
    pub image: Option<ImageAttachment>,
    pub audio: Option<AudioAttachment>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentError {
    InvalidFormat,
}

impl core::fmt::Display for AttachmentError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "malformed attachment field")
    }
}

impl core::error::Error for AttachmentError {}

impl From<msgpack::Error> for AttachmentError {
    fn from(_: msgpack::Error) -> Self {
        Self::InvalidFormat
    }
}

impl MessageAttachments {
    pub fn into_fields(self) -> Vec<Field> {
        let mut fields = Vec::new();
        if !self.files.is_empty() {
            let mut value = Vec::new();
            msgpack::array(&mut value, self.files.len());
            for file in self.files {
                msgpack::array(&mut value, 2);
                msgpack::string(&mut value, &file.name);
                msgpack::bin(&mut value, &file.data);
            }
            fields.push((FIELD_FILE_ATTACHMENTS, value));
        }
        if let Some(image) = self.image {
            let mut value = Vec::new();
            msgpack::array(&mut value, 2);
            msgpack::string(&mut value, &image.format);
            msgpack::bin(&mut value, &image.data);
            fields.push((FIELD_IMAGE, value));
        }
        if let Some(audio) = self.audio {
            let mut value = Vec::new();
            msgpack::array(&mut value, 2);
            msgpack::uint(&mut value, audio.mode as u64);
            msgpack::bin(&mut value, &audio.data);
            fields.push((FIELD_AUDIO, value));
        }
        fields
    }

    pub fn from_fields(fields: &[Field]) -> Result<Self, AttachmentError> {
        let mut attachments = Self::default();
        for (key, value) in fields {
            match *key {
                FIELD_FILE_ATTACHMENTS => attachments.files = decode_files(value)?,
                FIELD_IMAGE => attachments.image = Some(decode_image(value)?),
                FIELD_AUDIO => attachments.audio = Some(decode_audio(value)?),
                _ => {}
            }
        }
        Ok(attachments)
    }
}

fn decode_files(value: &[u8]) -> Result<Vec<FileAttachment>, AttachmentError> {
    let mut position = 0;
    let count = msgpack::array_len(value, &mut position)?;
    let mut files = Vec::with_capacity(count);
    for _ in 0..count {
        if msgpack::array_len(value, &mut position)? != 2 {
            return Err(AttachmentError::InvalidFormat);
        }
        let name = String::from(msgpack::read_str(value, &mut position)?);
        let data = msgpack::read_bin(value, &mut position)?.to_vec();
        files.push(FileAttachment { name, data });
    }
    finish(value, position)?;
    Ok(files)
}

fn decode_image(value: &[u8]) -> Result<ImageAttachment, AttachmentError> {
    let mut position = 0;
    if msgpack::array_len(value, &mut position)? != 2 {
        return Err(AttachmentError::InvalidFormat);
    }
    let format = String::from(msgpack::read_str(value, &mut position)?);
    let data = msgpack::read_bin(value, &mut position)?.to_vec();
    finish(value, position)?;
    Ok(ImageAttachment { format, data })
}

fn decode_audio(value: &[u8]) -> Result<AudioAttachment, AttachmentError> {
    let mut position = 0;
    if msgpack::array_len(value, &mut position)? != 2 {
        return Err(AttachmentError::InvalidFormat);
    }
    let mode = u8::try_from(msgpack::read_uint(value, &mut position)?)
        .map_err(|_| AttachmentError::InvalidFormat)?;
    let data = msgpack::read_bin(value, &mut position)?.to_vec();
    finish(value, position)?;
    Ok(AudioAttachment { mode, data })
}

fn finish(value: &[u8], position: usize) -> Result<(), AttachmentError> {
    if position == value.len() {
        Ok(())
    } else {
        Err(AttachmentError::InvalidFormat)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::AUDIO_MODE_CUSTOM;
    use alloc::vec;

    #[test]
    fn standard_attachment_fields_round_trip() {
        let expected = MessageAttachments {
            files: vec![FileAttachment {
                name: "notes.txt".into(),
                data: b"hello".to_vec(),
            }],
            image: Some(ImageAttachment {
                format: "png".into(),
                data: vec![1, 2, 3],
            }),
            audio: Some(AudioAttachment {
                mode: AUDIO_MODE_CUSTOM,
                data: vec![4, 5, 6],
            }),
        };
        let fields = expected.clone().into_fields();
        assert_eq!(MessageAttachments::from_fields(&fields).unwrap(), expected);
    }

    #[test]
    fn file_field_matches_python_lxmf_shape() {
        let fields = MessageAttachments {
            files: vec![FileAttachment {
                name: "a.txt".into(),
                data: vec![1, 2],
            }],
            ..MessageAttachments::default()
        }
        .into_fields();
        assert_eq!(
            fields,
            vec![(
                FIELD_FILE_ATTACHMENTS,
                vec![0x91, 0x92, 0xa5, b'a', b'.', b't', b'x', b't', 0xc4, 0x02, 0x01, 0x02]
            )]
        );
    }

    #[test]
    fn image_and_audio_fields_match_python_lxmf_shapes() {
        let fields = MessageAttachments {
            image: Some(ImageAttachment {
                format: "png".into(),
                data: vec![1, 2, 3],
            }),
            audio: Some(AudioAttachment {
                mode: AUDIO_MODE_CUSTOM,
                data: vec![4, 5, 6],
            }),
            ..MessageAttachments::default()
        }
        .into_fields();
        assert_eq!(
            fields,
            vec![
                (
                    FIELD_IMAGE,
                    vec![0x92, 0xa3, b'p', b'n', b'g', 0xc4, 0x03, 0x01, 0x02, 0x03]
                ),
                (
                    FIELD_AUDIO,
                    vec![0x92, 0xcc, 0xff, 0xc4, 0x03, 0x04, 0x05, 0x06]
                ),
            ]
        );
    }
}
