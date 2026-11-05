//! Looking for a character its font doesn't have, the text system opens every font
//! on the computer in turn and keeps them open, which is hundreds of megabytes of
//! font files sitting in this process. Now and then this lets go of the pages: what's
//! needed again comes straight back from the kernel's file cache.

use std::time::Duration;

const FIRST: Duration = Duration::from_secs(20);
const EVERY: Duration = Duration::from_secs(60);

/// A read-only mapping of a font file, from a line of /proc/self/maps.
fn font_range(line: &str) -> Option<(usize, usize)> {
    let mut parts = line.split_whitespace();
    let range = parts.next()?;
    let perms = parts.next()?;
    let path = parts.nth(3)?;
    if !perms.starts_with("r--") || !path.ends_with(".ttf") && !path.ends_with(".otf") && !path.ends_with(".ttc") {
        return None;
    }
    let (start, end) = range.split_once('-')?;
    Some((usize::from_str_radix(start, 16).ok()?, usize::from_str_radix(end, 16).ok()?))
}

#[cfg(target_os = "linux")]
pub fn start() {
    std::thread::spawn(|| {
        std::thread::sleep(FIRST);
        loop {
            if let Ok(maps) = std::fs::read_to_string("/proc/self/maps") {
                for (start, end) in maps.lines().filter_map(font_range) {
                    // SAFETY: a read-only file mapping holds nothing of ours. Its pages
                    // read the same when they're next touched, from the file.
                    unsafe { libc::madvise(start as *mut libc::c_void, end - start, libc::MADV_DONTNEED) };
                }
            }
            std::thread::sleep(EVERY);
        }
    });
}

#[cfg(not(target_os = "linux"))]
pub fn start() {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_read_only_font_files_count() {
        assert_eq!(font_range("7f00-7f10 r--s 00000000 103:02 1 /usr/share/fonts/a.ttf"), Some((0x7f00, 0x7f10)));
        assert_eq!(font_range("7f00-7f10 r--p 00000000 103:02 1 /home/x/.fonts/a font.otf"), None, "a space in the path isn't matched");
        assert_eq!(font_range("7f00-7f10 r--p 00000000 103:02 1 /usr/lib/libc.so.6"), None);
        assert_eq!(font_range("7f00-7f10 rw-p 00000000 103:02 1 /usr/share/fonts/a.ttf"), None);
        assert_eq!(font_range("7f00-7f10 rw-p 00000000 00:00 0"), None);
    }
}
