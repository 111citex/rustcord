gpui-base from gpui-kit (Apache-2.0), commit 3a14284, with one change: `InputState::set_text_styles`, so a plain input can style ranges of its text (the message box's live markdown).

gpui-fps from gpui-kit (Apache-2.0), commit 3a14284, with one change: the FPS readout counts frames shown. Presents within half a refresh of the last one count once (mailbox mode shows only the last), the rate is held to the panel's, and once a whole second has been watched it is frames in that second, so quiet time counts as no frames.
