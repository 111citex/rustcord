<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/brand/logo-white.png">
    <img src="assets/brand/logo-black.png" alt="rustcord logo" width="128">
  </picture>
</p>

<h1 align="center">rustcord</h1>

<p align="center">An unofficial, native Discord client written in Rust with GPUI.</p>

> [!CAUTION]
> **We do not recommend using rustcord with an account you care about.**
> Using it can put your account at risk, up to and including suspension or a permanent ban.
>
> This project exists **purely for educational purposes**. If you use it anyway, you do so entirely at your own risk.
> **The authors and contributors are not responsible for anything that happens to your account, your data, or anything else as a result of using it.**

## Please read this first

**Not affiliated with Discord.** rustcord is an independent project. It is not made, endorsed, or supported by Discord Inc. "Discord" is a trademark of Discord Inc.

**What rustcord is.** rustcord is not a modified version of Discord's app. It doesn't patch, inject into, or run any of Discord's client code. It's a separate program: a user interface on top of Discord's API.

**That does not make it allowed.** Discord's API for user accounts isn't a public API, and rustcord identifies itself to Discord as a web browser so that it works. Discord's [Terms of Service](https://discord.com/terms) and [Community Guidelines](https://discord.com/guidelines) still apply to you, and they may not permit third-party clients at all. It's your responsibility to read them and to make sure you don't break them. No third-party client can promise your account is safe, and neither do we.

**No paid features for free.** rustcord doesn't unlock Nitro or any other paid feature, and patches that do won't be accepted.

**Please don't sell this.** The license lets you, but we strongly suggest you don't. rustcord is built on, and depends entirely on, Discord's API, which can change or be closed to it at any time, and selling it adds risk for you and for the people who buy it. This is a request, not a license term: it doesn't change your rights under the GPL.

## Status

Early and rough, and expect bugs.

- **Works:** servers, DMs and group chats, messages, replies, attachments, reactions, profiles, themes, desktop notifications.
- **Voice:** listening only, with end-to-end encryption (DAVE). You can't talk yet.
- **Not yet:** talking in voice, screen share, calls in DMs, search, and more.

## Building

You need a stable Rust toolchain, plus your platform's usual GPU, audio, and font development libraries. It's developed and tested on Arch Linux with Hyprland only, so other platforms may not work.

```sh
cargo build --release
./target/release/rustcord
```

Your login token is kept in your system keyring. Never share it with anyone, and never paste it into a bug report.

## License

Copyright (C) 2026 the rustcord contributors.

rustcord is free software: you can redistribute it and/or modify it under the terms of the GNU General Public License as published by the Free Software Foundation, either version 3 of the License, or (at your option) any later version. See [LICENSE](LICENSE).

rustcord is distributed in the hope that it will be useful, but **without any warranty**, without even the implied warranty of merchantability or fitness for a particular purpose. See sections 15 and 16 of the license.

Third-party code and assets keep their own licenses:

- `vendor/` and `crates/alhazen-gpui`: see the license files inside each.
- `assets/name-fonts`: SIL Open Font License 1.1, see [its notice](assets/name-fonts/LICENSE.md).
- `themes/`: color themes that come from [gpui-component](https://github.com/longbridge/gpui-component) (Apache-2.0), plus our own changes.

## Credits

rustcord was designed and built by me. i set the whole direction, tested it down to the pixel, and wrote or debugged parts of it by hand, including the transition code. [Claude](https://claude.com/claude-code) wrote a lot of the code alongside me.
