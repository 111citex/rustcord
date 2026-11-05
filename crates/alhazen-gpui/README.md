# alhazen-gpui-pre

**非官方移植**：[alhazen-gpui](https://github.com/alhazen-rs/alhazen-gpui) 0.6.0 的本地移植，绑定
`gpui-pre 0.3.8`，与上游项目没有隶属关系。

上游绑定的是 `gpui 0.2.2`，与 `gpui-kit 0.7.x` 使用的 `gpui-pre 0.3.8` 不是同一个 crate。
本 crate 只改了 GPUI 适配层：`Element::paint` 的 `paint_image` 签名、`VideoFrame` 使用
`alhazen-core 0.6.0` 的 `Cpu` 变体，并移除了上游依赖的 `frame_formats`。上游 0.6.0 的
`videotoolbox` feature 在 crates.io 的 `alhazen-core 0.6.0` 中不存在，本 crate 没有暴露它。

## 构建要求

`alhazen-core` 的 `native` feature 负责 MP4/MOV demux，会拉入 `rav1d`，构建期需要 `nasm`：

```bash
sudo apt install nasm
```

运行时解码：Windows 走 Media Foundation，Linux 走 NVDEC 或用户安装的 `ffmpeg`；
macOS 在 0.6.0 下 H.264/HEVC 需要 `ffmpeg`（该版本尚未实现 VideoToolbox）。

## 用法

```rust
let video = cx.new(|cx| VideoPlayer::new(Source::parse(path)?, PlayerConfig::default(), cx));
// render:
video_view(video.clone()).size_full()
```

## 来源

代码来自 [alhazen-rs/alhazen-gpui](https://github.com/alhazen-rs/alhazen-gpui)，Apache-2.0。
