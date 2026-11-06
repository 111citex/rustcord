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

/// A GIF starred in the GIF picker. `FrecencyUserSettings` (settings-proto type 2)
/// field 2 is `FavoriteGIFs { map<string, FavoriteGIF> gifs = 1; bool hide_tooltip = 2; }`,
/// keyed by the GIF's page address; a `FavoriteGIF` is `{ GIFType format = 1; string src
/// = 2; uint32 width = 3; uint32 height = 4; uint32 order = 5; }`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FavoriteGif {
    /// The page address: what gets sent, and the map's key.
    pub url: String,
    /// The video (or picture) itself.
    pub src: String,
    /// 1 a picture, 2 a video.
    pub format: u32,
    pub width: u32,
    pub height: u32,
    /// Higher is starred later.
    pub order: u32,
}

const FAVORITE_GIFS: u64 = 2;

fn favorite_from(bytes: &[u8], url: String) -> FavoriteGif {
    let mut gif = FavoriteGif { url, ..Default::default() };
    for (field, value) in Fields::new(bytes) {
        match (field, value) {
            (1, Value::Varint(v)) => gif.format = v as u32,
            (2, Value::Bytes(s)) => gif.src = String::from_utf8_lossy(s).into_owned(),
            (3, Value::Varint(v)) => gif.width = v as u32,
            (4, Value::Varint(v)) => gif.height = v as u32,
            (5, Value::Varint(v)) => gif.order = v as u32,
            _ => {}
        }
    }
    gif
}

/// The starred GIFs in a `FrecencyUserSettings`, latest first.
pub fn favorite_gifs(proto: &[u8]) -> Vec<FavoriteGif> {
    let mut gifs = Vec::new();
    for (field, value) in Fields::new(proto) {
        let (FAVORITE_GIFS, Value::Bytes(favorites)) = (field, value) else { continue };
        for (field, value) in Fields::new(favorites) {
            let (1, Value::Bytes(entry)) = (field, value) else { continue };
            let (mut key, mut body) = (None, &[][..]);
            for (field, value) in Fields::new(entry) {
                match (field, value) {
                    (1, Value::Bytes(k)) => key = Some(String::from_utf8_lossy(k).into_owned()),
                    (2, Value::Bytes(v)) => body = v,
                    _ => {}
                }
            }
            if let Some(key) = key {
                gifs.push(favorite_from(body, key));
            }
        }
    }
    gifs.sort_by(|a, b| b.order.cmp(&a.order));
    gifs
}

fn put_varint(mut v: u64, out: &mut Vec<u8>) {
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

fn put_bytes(field: u64, bytes: &[u8], out: &mut Vec<u8>) {
    put_varint(field << 3 | 2, out);
    put_varint(bytes.len() as u64, out);
    out.extend_from_slice(bytes);
}

fn put_number(field: u64, value: u64, out: &mut Vec<u8>) {
    put_varint(field << 3, out);
    put_varint(value, out);
}

/// Each top-level field of `bytes` as (number, its whole encoding). `None` if malformed.
fn raw_fields(bytes: &[u8]) -> Option<Vec<(u64, &[u8])>> {
    let mut fields = Fields::new(bytes);
    let mut out = Vec::new();
    while !fields.rest.is_empty() {
        let start = bytes.len() - fields.rest.len();
        let (number, _) = fields.next()?;
        out.push((number, &bytes[start..bytes.len() - fields.rest.len()]));
    }
    Some(out)
}

/// `proto` with its starred GIFs replaced by `gifs`, and everything else in it (emoji
/// and sticker history, the tooltip flag, fields this doesn't know) kept byte for byte.
/// `None` if `proto` can't be read: then nothing should be written.
pub fn with_favorite_gifs(proto: &[u8], gifs: &[FavoriteGif]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut kept_inside = Vec::new();
    for (number, raw) in raw_fields(proto)? {
        if number != FAVORITE_GIFS {
            out.extend_from_slice(raw);
            continue;
        }
        let Some((_, Value::Bytes(inner))) = Fields::new(raw).next() else { return None };
        for (number, raw) in raw_fields(inner)? {
            if number != 1 {
                kept_inside.extend_from_slice(raw);
            }
        }
    }
    let mut favorites = Vec::new();
    for gif in gifs {
        let mut body = Vec::new();
        put_number(1, u64::from(gif.format), &mut body);
        put_bytes(2, gif.src.as_bytes(), &mut body);
        put_number(3, u64::from(gif.width), &mut body);
        put_number(4, u64::from(gif.height), &mut body);
        put_number(5, u64::from(gif.order), &mut body);
        let mut entry = Vec::new();
        put_bytes(1, gif.url.as_bytes(), &mut entry);
        put_bytes(2, &body, &mut entry);
        put_bytes(1, &entry, &mut favorites);
    }
    favorites.extend_from_slice(&kept_inside);
    put_bytes(FAVORITE_GIFS, &favorites, &mut out);
    Some(out)
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
    fn starred_gifs_change_and_the_rest_stays() {
        let gif = |url: &str, order| FavoriteGif { url: url.into(), src: format!("{url}.mp4"), format: 2, width: 498, height: 280, order };
        // Something else first (emoji history, say), then favourites with the tooltip flag.
        let mut proto = Vec::new();
        field_bytes(4, b"emoji history", &mut proto);
        let start = with_favorite_gifs(&proto, &[gif("https://tenor.com/a", 1)]).unwrap();
        let mut tooltip = Vec::new();
        varint(2 << 3, &mut tooltip);
        varint(1, &mut tooltip);
        // The flag sits inside field 2: put it there, as Discord would.
        let mut with_flag = Vec::new();
        for (number, raw) in raw_fields(&start).unwrap() {
            if number == 2 {
                let Some((_, Value::Bytes(inner))) = Fields::new(raw).next() else { panic!() };
                let mut inner = inner.to_vec();
                inner.extend_from_slice(&tooltip);
                field_bytes(2, &inner, &mut with_flag);
            } else {
                with_flag.extend_from_slice(raw);
            }
        }
        assert_eq!(favorite_gifs(&with_flag), vec![gif("https://tenor.com/a", 1)]);
        let both = with_favorite_gifs(&with_flag, &[gif("https://tenor.com/a", 1), gif("https://tenor.com/b", 2)]).unwrap();
        assert_eq!(favorite_gifs(&both).iter().map(|g| g.url.as_str()).collect::<Vec<_>>(), vec!["https://tenor.com/b", "https://tenor.com/a"], "latest first");
        let none = with_favorite_gifs(&both, &[]).unwrap();
        assert!(favorite_gifs(&none).is_empty());
        assert!(none.starts_with(&proto), "the other field is untouched");
        assert!(none.ends_with(&tooltip), "so is the tooltip flag");
        assert!(with_favorite_gifs(&[0xff], &[]).is_none(), "unreadable: write nothing");
    }

    #[test]
    fn garbage_gives_nothing() {
        assert!(guild_folders("not base64!").is_empty());
        assert!(guild_folders("").is_empty());
    }
}
