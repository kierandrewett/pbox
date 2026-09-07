//! Bounded RFB 3.8 client for the recipe's loopback, SecurityTypes=None server.
//! Wire format: https://www.rfc-editor.org/rfc/rfc6143 (sections 7.3–7.7).
use anyhow::{Context, Result, bail, ensure};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const MAX_PIXELS: usize = 16_777_216;
const MAX_TEXT: usize = 1_048_576;

pub(super) struct Client<S> {
    stream: S,
    pub width: u16,
    pub height: u16,
}

impl<S: AsyncRead + AsyncWrite + Unpin> Client<S> {
    pub async fn connect(mut stream: S) -> Result<Self> {
        let mut version = [0; 12];
        stream.read_exact(&mut version).await?;
        ensure!(
            &version == b"RFB 003.008\n",
            "desktop control requires RFB 3.8"
        );
        stream.write_all(b"RFB 003.008\n").await?;
        let count = stream.read_u8().await? as usize;
        ensure!(count > 0, "VNC server refused the connection");
        let mut security = vec![0; count];
        stream.read_exact(&mut security).await?;
        ensure!(
            security.contains(&1),
            "desktop control requires the recipe's loopback VNC backend (SecurityTypes=None)"
        );
        stream.write_all(&[1]).await?;
        ensure!(
            stream.read_u32().await? == 0,
            "VNC security negotiation failed"
        );
        // Shared connection: leave existing viewers attached.
        stream.write_all(&[1]).await?;
        let width = stream.read_u16().await?;
        let height = stream.read_u16().await?;
        ensure!(
            width > 0 && height > 0 && width as usize * height as usize <= MAX_PIXELS,
            "invalid or oversized desktop dimensions"
        );
        let mut format = [0; 16];
        stream.read_exact(&mut format).await?;
        skip_text(&mut stream).await?;
        // 32 bits, little endian, true colour; bytes R,G,B,padding.
        stream
            .write_all(&[
                0, 0, 0, 0, 32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 0, 8, 16, 0, 0, 0,
            ])
            .await?;
        // Request only raw rectangles. Cursor is rendered by the server.
        stream.write_all(&[2, 0, 0, 1, 0, 0, 0, 0]).await?;
        Ok(Self {
            stream,
            width,
            height,
        })
    }

    pub fn check_point(&self, x: u16, y: u16) -> Result<()> {
        ensure!(
            x < self.width && y < self.height,
            "point ({x}, {y}) is outside desktop {}x{}",
            self.width,
            self.height
        );
        Ok(())
    }

    pub async fn pointer(&mut self, x: u16, y: u16, buttons: u8) -> Result<()> {
        self.check_point(x, y)?;
        let mut message = vec![5, buttons];
        message.extend(x.to_be_bytes());
        message.extend(y.to_be_bytes());
        self.stream.write_all(&message).await?;
        Ok(())
    }

    async fn key(&mut self, key: u32, down: bool) -> Result<()> {
        let mut message = vec![4, u8::from(down), 0, 0];
        message.extend(key.to_be_bytes());
        self.stream.write_all(&message).await?;
        Ok(())
    }

    pub async fn chord(&mut self, keys: &[u32]) -> Result<()> {
        for &key in keys {
            self.key(key, true).await?;
        }
        for &key in keys.iter().rev() {
            self.key(key, false).await?;
        }
        Ok(())
    }

    /// A full update is also an ordered round trip after input. It does not
    /// guarantee that an application has finished responding to that input.
    pub async fn screen(&mut self) -> Result<Vec<u8>> {
        let mut request = vec![3, 0, 0, 0, 0, 0];
        request.extend(self.width.to_be_bytes());
        request.extend(self.height.to_be_bytes());
        self.stream.write_all(&request).await?;
        self.stream.flush().await?;
        let pixels = self.width as usize * self.height as usize;
        let mut rgba = vec![0; pixels * 4];
        let mut covered = vec![false; pixels];
        let mut remaining = pixels;
        loop {
            match self.stream.read_u8().await? {
                0 => {
                    self.stream.read_u8().await?;
                    let count = self.stream.read_u16().await?;
                    for _ in 0..count {
                        let x = self.stream.read_u16().await? as usize;
                        let y = self.stream.read_u16().await? as usize;
                        let w = self.stream.read_u16().await? as usize;
                        let h = self.stream.read_u16().await? as usize;
                        let encoding = self.stream.read_i32().await?;
                        ensure!(encoding == 0, "unsupported VNC encoding {encoding}");
                        ensure!(
                            w > 0
                                && h > 0
                                && x + w <= self.width as usize
                                && y + h <= self.height as usize,
                            "invalid VNC rectangle"
                        );
                        for row in y..y + h {
                            let start = row * self.width as usize + x;
                            self.stream
                                .read_exact(&mut rgba[start * 4..(start + w) * 4])
                                .await?;
                            for seen in &mut covered[start..start + w] {
                                if !*seen {
                                    *seen = true;
                                    remaining -= 1;
                                }
                            }
                        }
                    }
                    if remaining == 0 {
                        return Ok(rgba);
                    }
                }
                2 => {} // Bell.
                3 => {
                    let mut padding = [0; 3];
                    self.stream.read_exact(&mut padding).await?;
                    skip_text(&mut self.stream).await?;
                }
                kind => bail!("unsupported VNC server message {kind}"),
            }
        }
    }
}

async fn skip_text<S: AsyncRead + Unpin>(stream: &mut S) -> Result<()> {
    let size = stream.read_u32().await? as usize;
    ensure!(size <= MAX_TEXT, "oversized VNC text message");
    let mut bytes = vec![0; size];
    stream.read_exact(&mut bytes).await?;
    Ok(())
}

pub(super) fn png(width: u16, height: u16, rgba: &[u8]) -> Result<Vec<u8>> {
    let rgb: Vec<u8> = rgba
        .chunks_exact(4)
        .flat_map(|p| p[..3].iter().copied())
        .collect();
    let mut output = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut output, width.into(), height.into());
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder
            .write_header()?
            .write_image_data(&rgb)
            .context("encode desktop PNG")?;
    }
    Ok(output)
}

pub(super) fn character(c: char) -> u32 {
    match c {
        '\n' | '\r' => 0xff0d,
        '\t' => 0xff09,
        c if (c as u32) <= 0xff => c as u32,
        c => 0x0100_0000 | c as u32,
    }
}

pub(super) fn chord(value: &str) -> Result<Vec<u32>> {
    let mut parts: Vec<&str> = value.split('+').collect();
    let name = parts.pop().context("empty key")?;
    let mut keys = Vec::new();
    for modifier in parts {
        let key = match modifier.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => 0xffe3,
            "alt" => 0xffe9,
            "shift" => 0xffe1,
            "super" | "meta" | "win" => 0xffeb,
            _ => bail!("unknown modifier {modifier:?}"),
        };
        ensure!(!keys.contains(&key), "duplicate modifier {modifier:?}");
        keys.push(key);
    }
    let lower = name.to_ascii_lowercase();
    let key = match lower.as_str() {
        "enter" | "return" => 0xff0d,
        "tab" => 0xff09,
        "escape" | "esc" => 0xff1b,
        "backspace" => 0xff08,
        "delete" => 0xffff,
        "insert" => 0xff63,
        "home" => 0xff50,
        "end" => 0xff57,
        "pageup" => 0xff55,
        "pagedown" => 0xff56,
        "left" => 0xff51,
        "up" => 0xff52,
        "right" => 0xff53,
        "down" => 0xff54,
        "space" => 0x20,
        "plus" => 0x2b,
        "minus" => 0x2d,
        _ if lower.starts_with('f')
            && lower[1..]
                .parse::<u32>()
                .is_ok_and(|n| (1..=12).contains(&n)) =>
        {
            0xffbd + lower[1..].parse::<u32>()?
        }
        _ if name.chars().count() == 1 => {
            // Ctrl+C means the ordinary c key with Control, not Shift+C.
            let c = if keys.is_empty() {
                name.chars().next()
            } else {
                lower.chars().next()
            };
            character(c.context("empty key")?)
        }
        _ => bail!(
            "unknown key {name:?}; use a character, Enter, Tab, Escape, arrows, F1–F12 or modifiers such as Ctrl+C"
        ),
    };
    keys.push(key);
    Ok(keys)
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use tokio::io::DuplexStream;

    pub async fn server(mut stream: DuplexStream) -> Vec<Vec<u8>> {
        stream.write_all(b"RFB 003.008\n").await.unwrap();
        let mut version = [0; 12];
        stream.read_exact(&mut version).await.unwrap();
        assert_eq!(&version, b"RFB 003.008\n");
        stream.write_all(&[1, 1]).await.unwrap();
        assert_eq!(stream.read_u8().await.unwrap(), 1);
        stream.write_all(&[0; 4]).await.unwrap();
        assert_eq!(stream.read_u8().await.unwrap(), 1, "must preserve viewers");
        stream.write_all(&[0, 4, 0, 2]).await.unwrap();
        stream.write_all(&[0; 16]).await.unwrap();
        stream.write_all(&[0; 4]).await.unwrap();
        let mut format = [0; 20];
        stream.read_exact(&mut format).await.unwrap();
        assert_eq!(
            &format[4..17],
            &[32, 24, 0, 1, 0, 255, 0, 255, 0, 255, 0, 8, 16]
        );
        let mut encodings = [0; 8];
        stream.read_exact(&mut encodings).await.unwrap();
        assert_eq!(encodings, [2, 0, 0, 1, 0, 0, 0, 0]);
        let mut events = Vec::new();
        loop {
            let kind = stream.read_u8().await.unwrap();
            let size = match kind {
                3 => 10,
                4 => 8,
                5 => 6,
                _ => panic!("bad event {kind}"),
            };
            let mut event = vec![0; size];
            event[0] = kind;
            stream.read_exact(&mut event[1..]).await.unwrap();
            if kind == 3 {
                assert_eq!(event, [3, 0, 0, 0, 0, 0, 0, 4, 0, 2]);
                break;
            }
            events.push(event);
        }
        // Bell and clipboard must not desynchronise framebuffer decoding.
        stream
            .write_all(&[2, 3, 0, 0, 0, 0, 0, 0, 3, b'a', b'b', b'c'])
            .await
            .unwrap();
        // Full screen in two non-overlapping raw rectangles/updates.
        for x in [0u16, 2] {
            stream.write_all(&[0, 0, 0, 1]).await.unwrap();
            stream.write_all(&x.to_be_bytes()).await.unwrap();
            stream
                .write_all(&[0, 0, 0, 2, 0, 2, 0, 0, 0, 0])
                .await
                .unwrap();
            for _ in 0..4 {
                stream.write_all(&[x as u8, 100, 200, 0]).await.unwrap();
            }
        }
        events
    }

    #[tokio::test]
    async fn handshake_events_and_fragmented_screen_round_trip() {
        let (local, remote) = tokio::io::duplex(64);
        let server = tokio::spawn(server(remote));
        let mut client = Client::connect(local).await.unwrap();
        client.chord(&chord("Ctrl+C").unwrap()).await.unwrap();
        client.pointer(3, 1, 1).await.unwrap();
        client.pointer(3, 1, 0).await.unwrap();
        assert!(client.pointer(4, 1, 0).await.is_err());
        let pixels = client.screen().await.unwrap();
        assert_eq!(
            &pixels[..16],
            &[
                0, 100, 200, 0, 0, 100, 200, 0, 2, 100, 200, 0, 2, 100, 200, 0
            ]
        );
        let events = server.await.unwrap();
        assert_eq!(events[0], [4, 1, 0, 0, 0, 0, 0xff, 0xe3]);
        assert_eq!(events[1], [4, 1, 0, 0, 0, 0, 0, b'c']);
        assert_eq!(events[2], [4, 0, 0, 0, 0, 0, 0, b'c']);
        assert_eq!(events[3], [4, 0, 0, 0, 0, 0, 0xff, 0xe3]);
        assert_eq!(events[4], [5, 1, 0, 3, 0, 1]);
        assert_eq!(events[5], [5, 0, 0, 3, 0, 1]);
        let encoded = png(4, 2, &pixels).unwrap();
        let mut decoder = png::Decoder::new(std::io::Cursor::new(encoded))
            .read_info()
            .unwrap();
        let mut bytes = vec![0; decoder.output_buffer_size().unwrap()];
        let info = decoder.next_frame(&mut bytes).unwrap();
        assert_eq!((info.width, info.height), (4, 2));
        assert_eq!(&bytes[..6], &[0, 100, 200, 0, 100, 200]);
    }

    #[test]
    fn key_names_unicode_and_invalid_chords() {
        assert_eq!(chord("Ctrl+Alt+Delete").unwrap(), [0xffe3, 0xffe9, 0xffff]);
        assert_eq!(chord("F12").unwrap(), [0xffc9]);
        assert_eq!(chord("é").unwrap(), [0xe9]);
        assert_eq!(chord("猫").unwrap(), [0x0100_732b]);
        assert_eq!(chord("Plus").unwrap(), [43]);
        for key in ["", "Ctrl+", "Ctrl+Ctrl+C", "F13", "Bogus+X", "Ctrl+Typo"] {
            assert!(chord(key).is_err(), "{key}");
        }
    }

    #[tokio::test]
    async fn reject_non_rfb_and_unsupported_versions() {
        for response in [b"HTTP/1.1 200".to_vec(), b"RFB 003.003\n".to_vec()] {
            let (local, mut remote) = tokio::io::duplex(128);
            remote.write_all(&response).await.unwrap();
            drop(remote);
            assert!(Client::connect(local).await.is_err());
        }
    }
}
