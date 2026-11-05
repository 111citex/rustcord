//! The parts of READY's `user_settings_proto` the app uses: server folders.
//!
//! The field is base64 of a `PreloadedUserSettings` protobuf. Field 14 is
//! `GuildFolders { repeated GuildFolder folders = 1; repeated fixed64
//! guild_positions = 2; }`, and a `GuildFolder` is `{ repeated fixed64
//! guild_ids = 1; Int64Value id = 2; StringValue name = 3; UInt64Value
//! color = 4; }` (the wrappers hold their value in field 1). Only those
//! fields are read; everything else is skipped by wire type.

use base64::Engine;

use crate::Id;

/// One entry of the server rail: a folder (`id` set, or several servers) or
/// a lone server (no `id`, one server).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GuildFolder {
    pub id: Option<i64>,
    pub name: Option<String>,
    pub color: Option<u32>,
    pub guild_ids: Vec<Id>,
}

/// The folders from `user_settings_proto`, in rail order. Empty when the blob
/// is missing or malformed.
pub fn guild_folders(blob: &str) -> Vec<GuildFolder> {
    let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(blob.trim()) else { return Vec::new() };
    let mut folders = Vec::new();
    for (field, value) in Fields::new(&bytes) {
        if let (14, Value::Bytes(group)) = (field, value) {
            for (field, value) in Fields::new(group) {
                if let (1, Value::Bytes(folder)) = (field, value) {
                    folders.push(folder_from(folder));
                }
            }
        }
    }
    folders
}

fn folder_from(bytes: &[u8]) -> GuildFolder {
    let mut folder = GuildFolder::default();
    for (field, value) in Fields::new(bytes) {
        match (field, value) {
            (1, Value::Fixed64(id)) => folder.guild_ids.push(Id(id)),
            // Packed repeated fixed64.
            (1, Value::Bytes(packed)) => {
                folder.guild_ids.extend(packed.chunks_exact(8).map(|c| Id(u64::from_le_bytes(c.try_into().unwrap()))))
            }
            (2, Value::Bytes(wrapper)) => folder.id = wrapped_varint(wrapper).map(|v| v as i64),
            (3, Value::Bytes(wrapper)) => {
                folder.name = Fields::new(wrapper).find_map(|(f, v)| match (f, v) {
                    (1, Value::Bytes(s)) => String::from_utf8(s.to_vec()).ok(),
                    _ => None,
                })
            }
            (4, Value::Bytes(wrapper)) => folder.color = wrapped_varint(wrapper).map(|v| v as u32),
            _ => {}
        }
    }
    folder
}

fn wrapped_varint(bytes: &[u8]) -> Option<u64> {
    Fields::new(bytes).find_map(|(f, v)| match (f, v) {
        (1, Value::Varint(v)) => Some(v),
        _ => None,
    })
}

enum Value<'a> {
    Varint(u64),
    Fixed64(u64),
    Bytes(&'a [u8]),
    Fixed32,
}

/// Walks a protobuf message's top-level fields; stops at the first malformed one.
struct Fields<'a> {
    rest: &'a [u8],
}

impl<'a> Fields<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Fields { rest: bytes }
    }

    fn varint(&mut self) -> Option<u64> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let (&byte, rest) = self.rest.split_first()?;
            self.rest = rest;
            value |= u64::from(byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Some(value);
            }
        }
        None
    }

    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.rest.len() < n {
            return None;
        }
        let (head, rest) = self.rest.split_at(n);
        self.rest = rest;
        Some(head)
    }
}

impl<'a> Iterator for Fields<'a> {
    type Item = (u64, Value<'a>);

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.is_empty() {
            return None;
        }
        let key = self.varint()?;
        let value = match key & 7 {
            0 => Value::Varint(self.varint()?),
            1 => Value::Fixed64(u64::from_le_bytes(self.take(8)?.try_into().ok()?)),
            2 => {
                let len = usize::try_from(self.varint()?).ok()?;
                Value::Bytes(self.take(len)?)
            }
            5 => {
                self.take(4)?;
                Value::Fixed32
            }
            _ => {
                self.rest = &[];
                return None;
            }
        };
        Some((key >> 3, value))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn varint(mut v: u64, out: &mut Vec<u8>) {
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }

    fn field_bytes(field: u64, bytes: &[u8], out: &mut Vec<u8>) {
        varint(field << 3 | 2, out);
        varint(bytes.len() as u64, out);
        out.extend_from_slice(bytes);
    }

    #[test]
    fn reads_folders_and_lone_servers() {
        // Folder 7 "pzs" (two servers, colour 0xff0000), then a lone server.
        let mut folder = Vec::new();
        for id in [11u64, 12] {
            varint(1 << 3 | 1, &mut folder);
            folder.extend_from_slice(&id.to_le_bytes());
        }
        let mut wrapper = Vec::new();
        varint(1 << 3, &mut wrapper);
        varint(7, &mut wrapper);
        field_bytes(2, &wrapper, &mut folder);
        let mut name = Vec::new();
        field_bytes(1, b"pzs", &mut name);
        field_bytes(3, &name, &mut folder);
        let mut color = Vec::new();
        varint(1 << 3, &mut color);
        varint(0xff0000, &mut color);
        field_bytes(4, &color, &mut folder);

        let mut lone = Vec::new();
        varint(1 << 3 | 1, &mut lone);
        lone.extend_from_slice(&13u64.to_le_bytes());

        let mut group = Vec::new();
        field_bytes(1, &folder, &mut group);
        field_bytes(1, &lone, &mut group);
        let mut settings = Vec::new();
        varint(1 << 3, &mut settings); // an unrelated varint field first
        varint(5, &mut settings);
        field_bytes(14, &group, &mut settings);

        let blob = base64::engine::general_purpose::STANDARD.encode(&settings);
        let folders = guild_folders(&blob);
        assert_eq!(folders.len(), 2);
        assert_eq!(folders[0].id, Some(7));
        assert_eq!(folders[0].name.as_deref(), Some("pzs"));
        assert_eq!(folders[0].color, Some(0xff0000));
        assert_eq!(folders[0].guild_ids, vec![Id(11), Id(12)]);
        assert_eq!(folders[1], GuildFolder { guild_ids: vec![Id(13)], ..Default::default() });
    }

    #[test]
    fn garbage_gives_nothing() {
        assert!(guild_folders("not base64!").is_empty());
        assert!(guild_folders("").is_empty());
    }
}
