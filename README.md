# Remote P2P

> **Cross-OS Peer-to-Peer Keyboard, Mouse, Clipboard & File Sharing over QUIC (Windows ↔ macOS)**
>
> *Điều khiển Chuột, Bàn phím, Clipboard và Copy/Paste File/Thư mục xuyên hệ điều hành (Windows ↔ macOS) qua mạng LAN P2P tốc độ cao (QUIC).*

---

## Language / Ngôn ngữ
- [English Guide](#english-guide)
- [Hướng dẫn Tiếng Việt](#huong-dan-tieng-viet)

---

<a name="english-guide"></a>
## English Guide

### Overview & Key Features
**Remote P2P** lets you seamlessly share a single mouse and keyboard across multiple computers (Windows and macOS) on the same LAN, with zero central server:
- **Zero-Config LAN Discovery (mDNS)** + **Manual IP:Port Connect** fallback.
- **Encrypted P2P Transport (QUIC / TLS)** with low-latency datagrams for mouse movement and reliable streams for clicks, keys, clipboard, and files.
- **Multi-Monitor & DPI Aware**: Supports multiple monitors per machine (including negative coordinates when secondary monitors are placed left/above primary) with **real-time display layout detection** and mini-screen topology visualization.
- **Cross-OS Key Mapping**:
  - `SmartErgonomic`: Automatically maps `Ctrl (Win) ↔ Cmd (Mac)` and `Win ↔ Ctrl`.
  - `Custom`: Fine-grained modifier & key remapping (e.g. `CapsLock → Esc`).
  - `RawHardware`: Keeps physical USB HID keys as-is.
- **Cross-OS Clipboard & File/Folder Copy-Paste**: Copy text, files, or entire folders on Windows (`Ctrl+C`) and paste directly on macOS (`Cmd+V`) or vice versa (staged via BLAKE3-verified chunked QUIC streams into `~/Downloads/P2P_Drop` on macOS and `%USERPROFILE%\Downloads\P2P_Drop` on Windows).
- **Bilingual UI (English / Tiếng Việt)** with instant language switcher and persistent settings (`config.json`).
- **Safety & Self-Healing**: Heartbeat lease watchdog, automatic local control recovery on disconnect/sleep, `Panic Reset (Local)` button & emergency hotkey, and persistent file logging (`p2pkm.log`).

### Prerequisites & Build

#### Windows
1. Install **Rust** (`rustup`): https://rustup.rs/
2. Build & run the GUI app:
   ```powershell
   cargo build --release --manifest-path apps/p2pkm-ui/src-tauri/Cargo.toml
   .\apps\p2pkm-ui\src-tauri\target\release\p2pkm-ui.exe
   ```
3. **Windows Firewall**: Allow `p2pkm-ui.exe` on Private/LAN networks (UDP port `51999` + mDNS UDP `5353`).

#### macOS
1. Install **Rust** (`rustup`) and **Xcode Command Line Tools** (`xcode-select --install`).
2. Build & run the GUI app:
   ```bash
   cargo build --release --manifest-path apps/p2pkm-ui/src-tauri/Cargo.toml
   ./apps/p2pkm-ui/src-tauri/target/release/p2pkm-ui
   ```
3. **macOS Accessibility Permission (Required)**:
   - Open **System Settings → Privacy & Security → Accessibility**.
   - Add and enable `p2pkm-ui`.
   - *(Note: If you rebuild `p2pkm-ui` from source, remove the old entry in Accessibility and re-add the newly built binary, then relaunch the app. If Accessibility is missing, Remote P2P will show a warning banner and lock outgoing control from Mac to prevent dual-cursor mirroring.)*

### How to Use
1. **Launch Remote P2P** on both machines on the same Wi-Fi/LAN.
   - Devices automatically appear under **Connected Devices** via mDNS.
   - If mDNS is blocked by your router/VPN, enter the peer's `<IP>:<Port>` (e.g. `192.168.88.111:51999`) and click **Connect**.
2. **Arrange Screen Topology**:
   - Use the dropdown next to each peer (`Left`, `Right`, `Top`, `Bottom`) relative to the current machine in the center.
3. **Switch Between Machines**:
   - Push your cursor past the shared screen edge by the configured **Overshoot** (default `100 px`) to control the adjacent machine.
   - Push back past the edge to return to your local machine.
4. **Copy/Paste Files & Folders**:
   - Select files or folders on Machine A → `Ctrl+C` (Win) / `Cmd+C` (Mac).
   - Move cursor to Machine B → `Ctrl+V` (Win) / `Cmd+V` (Mac).
5. **Controls & Troubleshooting**:
   - **🔄 Refresh**: Immediately probes all peers (500ms timeout) to remove offline peers or reconnect peers that just came online.
   - **Panic Reset (Local)** / Emergency Hotkey:
     - **Windows**: `Ctrl + Alt + Shift + Esc`
     - **macOS**: `Cmd + Option + Ctrl + Esc`
     - Forces control back to Local, unhides/unparks the cursor, and releases any held keys/buttons.
   - **Allow Outgoing Control**: Uncheck on a client machine if you only want one-way control (e.g., Windows → Mac only).
   - **Logs**:
     - Windows: `%APPDATA%\p2pkm\p2pkm.log`
     - macOS: `~/Library/Application Support/p2pkm/p2pkm.log`

---

<a name="huong-dan-tieng-viet"></a>
## Hướng dẫn Tiếng Việt

### Tổng quan & Tính năng chính
**Remote P2P** cho phép bạn dùng chung **một bộ Chuột & Bàn phím** để điều khiển liền mạch nhiều máy tính (Windows ↔ macOS) trong cùng mạng LAN theo kiến trúc P2P trực tiếp (không cần server trung gian):
- **Tự động tìm thiết bị trong LAN (mDNS)** + hỗ trợ **kết nối thủ công qua IP:Port**.
- **Truyền tải QUIC mã hóa TLS**: Độ trễ cực thấp cho chuột, kết hợp luồng tin cậy cho click, bàn phím, clipboard và truyền file.
- **Hỗ trợ đa màn hình (Multi-Monitor) & DPI Scaling**: Nhận diện đầy đủ nhiều màn hình trên mỗi máy (kể cả màn hình phụ đặt bên trái/trên với tọa độ âm) và **tự động cập nhật sơ đồ màn hình theo thời gian thực** khi cắm/rút màn hình.
- **Ánh xạ phím thông minh Cross-OS**:
  - `SmartErgonomic`: Tự động đổi `Ctrl (Win) ↔ Cmd (Mac)` và `Win ↔ Ctrl`.
  - `Custom`: Tùy chỉnh chi tiết từng phím chức năng (ví dụ `CapsLock → Esc`).
  - `RawHardware`: Giữ nguyên mã phím vật lý.
- **Copy/Paste Text, File & Thư mục xuyên OS**: Copy file/folder bên Windows (`Ctrl+C`) rồi sang macOS bấm `Cmd+V` để dán trực tiếp (hoặc ngược lại). File được truyền qua QUIC kiểm tra toàn vẹn BLAKE3 vào thư mục `~/Downloads/P2P_Drop` (Mac) hoặc `%USERPROFILE%\Downloads\P2P_Drop` (Windows).
- **Giao diện Song ngữ (Tiếng Việt / English)** với nút chuyển nhanh và tự động lưu cấu hình (`config.json`).
- **An toàn & Tự phục hồi**: Tự thu hồi quyền điều khiển khi mất kết nối, chống App Nap trên macOS, nút **Panic Reset (Local)** + phím tắt khẩn cấp, và tự ghi log ra file `p2pkm.log`.

### Yêu cầu & Cài đặt (Build)

#### Trên Windows
1. Cài đặt **Rust** (`rustup`): https://rustup.rs/
2. Build và chạy ứng dụng:
   ```powershell
   cargo build --release --manifest-path apps/p2pkm-ui/src-tauri/Cargo.toml
   .\apps\p2pkm-ui\src-tauri\target\release\p2pkm-ui.exe
   ```
3. **Windows Firewall**: Cho phép `p2pkm-ui.exe` truy cập mạng LAN/Private (cổng UDP `51999` và mDNS UDP `5353`).

#### Trên macOS
1. Cài đặt **Rust** (`rustup`) và **Xcode Command Line Tools** (`xcode-select --install`).
2. Build và chạy ứng dụng:
   ```bash
   cargo build --release --manifest-path apps/p2pkm-ui/src-tauri/Cargo.toml
   ./apps/p2pkm-ui/src-tauri/target/release/p2pkm-ui
   ```
3. **Cấp quyền Accessibility (Bắt buộc trên macOS)**:
   - Mở **System Settings → Privacy & Security → Accessibility** (Trợ năng).
   - Thêm và bật công tắc cho `p2pkm-ui`.
   - *(Lưu ý: Mỗi lần build lại binary `p2pkm-ui` mới, hãy xóa dòng cũ trong Accessibility, thêm lại file binary vừa build và mở lại app. Nếu thiếu quyền này, app sẽ hiện cảnh báo đỏ và tự khóa chiều điều khiển đi từ Mac để tránh lỗi điều khiển song song 2 máy.)*

### Hướng dẫn sử dụng
1. **Mở Remote P2P** trên cả 2 máy (cùng mạng Wi-Fi/LAN).
   - Các máy sẽ tự hiện trong **Danh sách thiết bị đang kết nối**.
   - Nếu mạng chặn mDNS, nhập `<IP>:<Port>` (vd: `192.168.88.111:51999`) rồi bấm **Kết nối**.
2. **Sắp xếp vị trí màn hình**:
   - Chọn vị trí của máy kia (`Bên trái`, `Bên phải`, `Phía trên`, `Phía dưới`) so với máy hiện tại ở chính giữa.
3. **Chuyển điều khiển giữa các máy**:
   - Đẩy chuột vượt qua mép màn hình tương ứng một khoảng **Overshoot** (mặc định `100 px`) để chuyển sang máy bên cạnh.
   - Đẩy ngược lại qua mép để quay về máy ban đầu.
4. **Copy/Paste File & Thư mục**:
   - Chọn file/folder ở máy A → bấm `Ctrl+C` (Win) hoặc `Cmd+C` (Mac).
   - Di chuyển chuột sang máy B → bấm `Ctrl+V` (Win) hoặc `Cmd+V` (Mac).
5. **Các nút & phím tắt hữu ích**:
   - **🔄 Làm mới (Refresh)**: Chủ động kiểm tra kết nối tới các máy (timeout 500ms) để gỡ ngay máy đã tắt hoặc kết nối lại máy vừa bật.
   - **Panic Reset (Local)** / Phím tắt khẩn cấp:
     - **Windows**: `Ctrl + Alt + Shift + Esc`
     - **macOS**: `Cmd + Option + Ctrl + Esc`
     - Dùng khi máy bên kia bị treo/rớt mạng hoặc kẹt phím để thu hồi ngay chuột & bàn phím về máy cục bộ.
   - **Cho phép máy này điều khiển máy khác (Outgoing Control)**: Có thể bỏ tích trên máy Client nếu bạn chỉ muốn điều khiển 1 chiều từ Host sang Client.
   - **File Log chẩn đoán lỗi**:
     - Windows: `%APPDATA%\p2pkm\p2pkm.log`
     - macOS: `~/Library/Application Support/p2pkm/p2pkm.log`
