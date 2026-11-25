use bytes::{Buf, BufMut, Bytes, BytesMut};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const PROTOCOL_VERSION: u8 = 1;
pub const MAGIC_BYTE: u8 = 0xA5;

/// 16-byte Peer identifier (UUID v4 bytes).
pub type PeerId = [u8; 16];

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProtoError {
    #[error("packet too short: expected at least {expected} bytes, got {actual}")]
    TooShort { expected: usize, actual: usize },
    #[error("invalid magic byte: {0:#x}")]
    InvalidMagic(u8),
    #[error("unsupported protocol version: {0}")]
    UnsupportedVersion(u8),
    #[error("unknown message type: {0:#x}")]
    UnknownMsgType(u8),
    #[error("invalid payload data")]
    InvalidPayload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[repr(u8)]
pub enum OsKind {
    Windows = 1,
    MacOS = 2,
    Linux = 3,
}

impl OsKind {
    pub fn from_u8(v: u8) -> Result<Self, ProtoError> {
        match v {
            1 => Ok(Self::Windows),
            2 => Ok(Self::MacOS),
            3 => Ok(Self::Linux),
            _ => Err(ProtoError::InvalidPayload),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum MouseButton {
    Left = 1,
    Right = 2,
    Middle = 3,
    X1 = 4,
    X2 = 5,
}

impl MouseButton {
    pub fn from_u8(v: u8) -> Result<Self, ProtoError> {
        match v {
            1 => Ok(Self::Left),
            2 => Ok(Self::Right),
            3 => Ok(Self::Middle),
            4 => Ok(Self::X1),
            5 => Ok(Self::X2),
            _ => Err(ProtoError::InvalidPayload),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ScreenEdge {
    Left = 0,
    Right = 1,
    Top = 2,
    Bottom = 3,
}

impl ScreenEdge {
    pub fn opposite(self) -> Self {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Left,
            Self::Top => Self::Bottom,
            Self::Bottom => Self::Top,
        }
    }

    pub fn from_u8(v: u8) -> Result<Self, ProtoError> {
        match v {
            0 => Ok(Self::Left),
            1 => Ok(Self::Right),
            2 => Ok(Self::Top),
            3 => Ok(Self::Bottom),
            _ => Err(ProtoError::InvalidPayload),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteDisplayRect {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
    pub scale_factor_x100: u16,
    pub is_primary: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Packet {
    /// Hot-path datagram (16 bytes total)
    MouseMove {
        seq: u32,
        dx: i16,
        dy: i16,
        timestamp_us: u32,
    },
    /// Reliable stream events
    MouseButton {
        seq: u32,
        button: MouseButton,
        pressed: bool,
    },
    MouseScroll {
        seq: u32,
        dx: i16,
        dy: i16,
    },
    /// Canonical USB HID Usage ID (Page 0x07 Keyboard/Keypad).
    KeyEvent {
        seq: u32,
        hid_usage: u16,
        pressed: bool,
    },
    Heartbeat {
        epoch: u64,
        timestamp_us: u32,
    },
    FocusRequest {
        epoch: u64,
        from_peer: PeerId,
        entry_edge: ScreenEdge,
        /// Normalized position along the shared edge in [0.0, 1.0].
        edge_ratio: f32,
    },
    FocusAck {
        epoch: u64,
        target_peer: PeerId,
    },
    FocusReject {
        epoch: u64,
        current_controller: PeerId,
    },
    FocusRevoke {
        epoch: u64,
        reason: u8, // 0 = normal return, 1 = local preemption, 2 = timeout/sleep
    },
    Hello {
        peer_id: PeerId,
        os: OsKind,
        hostname: String,
    },
    DisplayLayout {
        peer_id: PeerId,
        logical_w: u32,
        logical_h: u32,
        physical_w: u32,
        physical_h: u32,
        displays: Vec<RemoteDisplayRect>,
    },
    ClipboardText {
        seq: u32,
        text: String,
    },
    ClipboardFilesStaged {
        seq: u32,
        root_names: Vec<String>,
    },
}

#[repr(u8)]
enum MsgType {
    MouseMove = 0x01,
    MouseButton = 0x02,
    MouseScroll = 0x03,
    KeyEvent = 0x04,
    Heartbeat = 0x05,
    FocusRequest = 0x10,
    FocusAck = 0x11,
    FocusReject = 0x12,
    FocusRevoke = 0x13,
    Hello = 0x20,
    DisplayLayout = 0x21,
    ClipboardText = 0x30,
    ClipboardFilesStaged = 0x31,
}

impl Packet {
    /// Encode packet into a compact binary representation.
    /// Header: [MAGIC_BYTE (1B)][PROTOCOL_VERSION (1B)][MsgType (1B)]
    pub fn encode(&self, buf: &mut BytesMut) {
        buf.put_u8(MAGIC_BYTE);
        buf.put_u8(PROTOCOL_VERSION);
        match self {
            Packet::MouseMove {
                seq,
                dx,
                dy,
                timestamp_us,
            } => {
                buf.put_u8(MsgType::MouseMove as u8);
                buf.put_u32(*seq);
                buf.put_i16(*dx);
                buf.put_i16(*dy);
                buf.put_u32(*timestamp_us);
            }
            Packet::MouseButton {
                seq,
                button,
                pressed,
            } => {
                buf.put_u8(MsgType::MouseButton as u8);
                buf.put_u32(*seq);
                buf.put_u8(*button as u8);
                buf.put_u8(u8::from(*pressed));
            }
            Packet::MouseScroll { seq, dx, dy } => {
                buf.put_u8(MsgType::MouseScroll as u8);
                buf.put_u32(*seq);
                buf.put_i16(*dx);
                buf.put_i16(*dy);
            }
            Packet::KeyEvent {
                seq,
                hid_usage,
                pressed,
            } => {
                buf.put_u8(MsgType::KeyEvent as u8);
                buf.put_u32(*seq);
                buf.put_u16(*hid_usage);
                buf.put_u8(u8::from(*pressed));
            }
            Packet::Heartbeat {
                epoch,
                timestamp_us,
            } => {
                buf.put_u8(MsgType::Heartbeat as u8);
                buf.put_u64(*epoch);
                buf.put_u32(*timestamp_us);
            }
            Packet::FocusRequest {
                epoch,
                from_peer,
                entry_edge,
                edge_ratio,
            } => {
                buf.put_u8(MsgType::FocusRequest as u8);
                buf.put_u64(*epoch);
                buf.put_slice(from_peer);
                buf.put_u8(*entry_edge as u8);
                buf.put_f32(*edge_ratio);
            }
            Packet::FocusAck { epoch, target_peer } => {
                buf.put_u8(MsgType::FocusAck as u8);
                buf.put_u64(*epoch);
                buf.put_slice(target_peer);
            }
            Packet::FocusReject {
                epoch,
                current_controller,
            } => {
                buf.put_u8(MsgType::FocusReject as u8);
                buf.put_u64(*epoch);
                buf.put_slice(current_controller);
            }
            Packet::FocusRevoke { epoch, reason } => {
                buf.put_u8(MsgType::FocusRevoke as u8);
                buf.put_u64(*epoch);
                buf.put_u8(*reason);
            }
            Packet::Hello {
                peer_id,
                os,
                hostname,
            } => {
                buf.put_u8(MsgType::Hello as u8);
                buf.put_slice(peer_id);
                buf.put_u8(*os as u8);
                let name_bytes = hostname.as_bytes();
                let len = name_bytes.len().min(255) as u8;
                buf.put_u8(len);
                buf.put_slice(&name_bytes[..len as usize]);
            }
            Packet::DisplayLayout {
                peer_id,
                logical_w,
                logical_h,
                physical_w,
                physical_h,
                displays,
            } => {
                buf.put_u8(MsgType::DisplayLayout as u8);
                buf.put_slice(peer_id);
                buf.put_u32(*logical_w);
                buf.put_u32(*logical_h);
                buf.put_u32(*physical_w);
                buf.put_u32(*physical_h);
                let count = displays.len().min(u8::MAX as usize) as u8;
                buf.put_u8(count);
                for d in displays.iter().take(count as usize) {
                    buf.put_i32(d.x);
                    buf.put_i32(d.y);
                    buf.put_u32(d.width);
                    buf.put_u32(d.height);
                    buf.put_u16(d.scale_factor_x100);
                    buf.put_u8(u8::from(d.is_primary));
                }
            }
            Packet::ClipboardText { seq, text } => {
                buf.put_u8(MsgType::ClipboardText as u8);
                buf.put_u32(*seq);
                let bytes = text.as_bytes();
                let len = bytes.len().min(1024 * 1024) as u32;
                buf.put_u32(len);
                buf.put_slice(&bytes[..len as usize]);
            }
            Packet::ClipboardFilesStaged { seq, root_names } => {
                buf.put_u8(MsgType::ClipboardFilesStaged as u8);
                buf.put_u32(*seq);
                let count = root_names.len().min(u16::MAX as usize) as u16;
                buf.put_u16(count);
                for name in root_names.iter().take(count as usize) {
                    let b = name.as_bytes();
                    let l = b.len().min(u16::MAX as usize) as u16;
                    buf.put_u16(l);
                    buf.put_slice(&b[..l as usize]);
                }
            }
        }
    }

    pub fn to_bytes(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(64);
        self.encode(&mut buf);
        buf.freeze()
    }

    pub fn decode(mut data: &[u8]) -> Result<Self, ProtoError> {
        if data.len() < 3 {
            return Err(ProtoError::TooShort {
                expected: 3,
                actual: data.len(),
            });
        }
        let magic = data.get_u8();
        if magic != MAGIC_BYTE {
            return Err(ProtoError::InvalidMagic(magic));
        }
        let ver = data.get_u8();
        if ver != PROTOCOL_VERSION {
            return Err(ProtoError::UnsupportedVersion(ver));
        }
        let msg_type = data.get_u8();

        match msg_type {
            x if x == MsgType::MouseMove as u8 => {
                ensure_len(data, 12)?;
                Ok(Packet::MouseMove {
                    seq: data.get_u32(),
                    dx: data.get_i16(),
                    dy: data.get_i16(),
                    timestamp_us: data.get_u32(),
                })
            }
            x if x == MsgType::MouseButton as u8 => {
                ensure_len(data, 6)?;
                Ok(Packet::MouseButton {
                    seq: data.get_u32(),
                    button: MouseButton::from_u8(data.get_u8())?,
                    pressed: data.get_u8() != 0,
                })
            }
            x if x == MsgType::MouseScroll as u8 => {
                ensure_len(data, 8)?;
                Ok(Packet::MouseScroll {
                    seq: data.get_u32(),
                    dx: data.get_i16(),
                    dy: data.get_i16(),
                })
            }
            x if x == MsgType::KeyEvent as u8 => {
                ensure_len(data, 7)?;
                Ok(Packet::KeyEvent {
                    seq: data.get_u32(),
                    hid_usage: data.get_u16(),
                    pressed: data.get_u8() != 0,
                })
            }
            x if x == MsgType::Heartbeat as u8 => {
                ensure_len(data, 12)?;
                Ok(Packet::Heartbeat {
                    epoch: data.get_u64(),
                    timestamp_us: data.get_u32(),
                })
            }
            x if x == MsgType::FocusRequest as u8 => {
                ensure_len(data, 29)?;
                let epoch = data.get_u64();
                let mut from_peer = [0u8; 16];
                data.copy_to_slice(&mut from_peer);
                let entry_edge = ScreenEdge::from_u8(data.get_u8())?;
                let edge_ratio = data.get_f32().clamp(0.0, 1.0);
                Ok(Packet::FocusRequest {
                    epoch,
                    from_peer,
                    entry_edge,
                    edge_ratio,
                })
            }
            x if x == MsgType::FocusAck as u8 => {
                ensure_len(data, 24)?;
                let epoch = data.get_u64();
                let mut target_peer = [0u8; 16];
                data.copy_to_slice(&mut target_peer);
                Ok(Packet::FocusAck { epoch, target_peer })
            }
            x if x == MsgType::FocusReject as u8 => {
                ensure_len(data, 24)?;
                let epoch = data.get_u64();
                let mut current_controller = [0u8; 16];
                data.copy_to_slice(&mut current_controller);
                Ok(Packet::FocusReject {
                    epoch,
                    current_controller,
                })
            }
            x if x == MsgType::FocusRevoke as u8 => {
                ensure_len(data, 9)?;
                Ok(Packet::FocusRevoke {
                    epoch: data.get_u64(),
                    reason: data.get_u8(),
                })
            }
            x if x == MsgType::Hello as u8 => {
                ensure_len(data, 18)?;
                let mut peer_id = [0u8; 16];
                data.copy_to_slice(&mut peer_id);
                let os = OsKind::from_u8(data.get_u8())?;
                let len = data.get_u8() as usize;
                ensure_len(data, len)?;
                let hostname = std::str::from_utf8(&data[..len])
                    .map_err(|_| ProtoError::InvalidPayload)?
                    .to_string();
                Ok(Packet::Hello {
                    peer_id,
                    os,
                    hostname,
                })
            }
            x if x == MsgType::ClipboardText as u8 => {
                ensure_len(data, 8)?;
                let seq = data.get_u32();
                let len = data.get_u32() as usize;
                ensure_len(data, len)?;
                let text = std::str::from_utf8(&data[..len])
                    .map_err(|_| ProtoError::InvalidPayload)?
                    .to_string();
                Ok(Packet::ClipboardText { seq, text })
            }
            x if x == MsgType::ClipboardFilesStaged as u8 => {
                ensure_len(data, 6)?;
                let seq = data.get_u32();
                let count = data.get_u16() as usize;
                let mut root_names = Vec::with_capacity(count);
                for _ in 0..count {
                    ensure_len(data, 2)?;
                    let l = data.get_u16() as usize;
                    ensure_len(data, l)?;
                    let name = std::str::from_utf8(&data[..l])
                        .map_err(|_| ProtoError::InvalidPayload)?
                        .to_string();
                    data.advance(l);
                    root_names.push(name);
                }
                Ok(Packet::ClipboardFilesStaged { seq, root_names })
            }
            x if x == MsgType::DisplayLayout as u8 => {
                ensure_len(data, 33)?;
                let mut peer_id = [0u8; 16];
                data.copy_to_slice(&mut peer_id);
                let logical_w = data.get_u32();
                let logical_h = data.get_u32();
                let physical_w = data.get_u32();
                let physical_h = data.get_u32();
                let count = data.get_u8() as usize;
                ensure_len(data, count * 19)?;
                let mut displays = Vec::with_capacity(count);
                for _ in 0..count {
                    displays.push(RemoteDisplayRect {
                        x: data.get_i32(),
                        y: data.get_i32(),
                        width: data.get_u32(),
                        height: data.get_u32(),
                        scale_factor_x100: data.get_u16(),
                        is_primary: data.get_u8() != 0,
                    });
                }
                Ok(Packet::DisplayLayout {
                    peer_id,
                    logical_w,
                    logical_h,
                    physical_w,
                    physical_h,
                    displays,
                })
            }
            other => Err(ProtoError::UnknownMsgType(other)),
        }
    }
}

fn ensure_len(data: &[u8], expected: usize) -> Result<(), ProtoError> {
    if data.len() < expected {
        Err(ProtoError::TooShort {
            expected,
            actual: data.len(),
        })
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_all_packets() {
        let peer_a = [1u8; 16];
        let peer_b = [2u8; 16];
        let packets = vec![
            Packet::MouseMove {
                seq: 42,
                dx: -15,
                dy: 27,
                timestamp_us: 123_456,
            },
            Packet::MouseButton {
                seq: 43,
                button: MouseButton::Right,
                pressed: true,
            },
            Packet::MouseScroll {
                seq: 44,
                dx: 0,
                dy: -120,
            },
            Packet::KeyEvent {
                seq: 45,
                hid_usage: 0x04, // 'A'
                pressed: true,
            },
            Packet::Heartbeat {
                epoch: 9,
                timestamp_us: 999,
            },
            Packet::FocusRequest {
                epoch: 10,
                from_peer: peer_a,
                entry_edge: ScreenEdge::Left,
                edge_ratio: 0.45,
            },
            Packet::FocusAck {
                epoch: 10,
                target_peer: peer_b,
            },
            Packet::FocusReject {
                epoch: 10,
                current_controller: peer_a,
            },
            Packet::FocusRevoke {
                epoch: 10,
                reason: 1,
            },
            Packet::Hello {
                peer_id: peer_a,
                os: OsKind::Windows,
                hostname: "WORKSTATION-WIN".into(),
            },
            Packet::DisplayLayout {
                peer_id: peer_a,
                logical_w: 3360,
                logical_h: 1080,
                physical_w: 3360,
                physical_h: 1080,
                displays: vec![
                    RemoteDisplayRect {
                        x: 0,
                        y: 0,
                        width: 1440,
                        height: 900,
                        scale_factor_x100: 100,
                        is_primary: true,
                    },
                    RemoteDisplayRect {
                        x: 1440,
                        y: 0,
                        width: 1920,
                        height: 1080,
                        scale_factor_x100: 100,
                        is_primary: false,
                    },
                ],
            },
            Packet::ClipboardText {
                seq: 99,
                text: "Xin chào P2P Mesh!".into(),
            },
            Packet::ClipboardFilesStaged {
                seq: 100,
                root_names: vec!["Project".into(), "notes.txt".into()],
            },
        ];

        for pkt in packets {
            let encoded = pkt.to_bytes();
            let decoded = Packet::decode(&encoded).expect("decode must succeed");
            assert_eq!(pkt, decoded);
        }
    }

    #[test]
    fn mouse_move_packet_is_15_bytes() {
        let pkt = Packet::MouseMove {
            seq: 1,
            dx: 5,
            dy: -5,
            timestamp_us: 100,
        };
        assert_eq!(pkt.to_bytes().len(), 15);
    }

    #[test]
    fn reject_invalid_magic_and_truncated() {
        assert_eq!(
            Packet::decode(&[0x00, 0x01, 0x01]),
            Err(ProtoError::InvalidMagic(0x00))
        );
        assert!(matches!(
            Packet::decode(&[MAGIC_BYTE, PROTOCOL_VERSION]),
            Err(ProtoError::TooShort { .. })
        ));
        assert_eq!(
            Packet::decode(&[MAGIC_BYTE, 99, 0x01]),
            Err(ProtoError::UnsupportedVersion(99))
        );
        assert_eq!(
            Packet::decode(&[MAGIC_BYTE, PROTOCOL_VERSION, 0xFE]),
            Err(ProtoError::UnknownMsgType(0xFE))
        );
    }
}
