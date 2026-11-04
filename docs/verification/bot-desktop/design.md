# Bot computer viewer

The right sidebar shows the bot's computer. Selecting the Computer tab checks its state without starting it. The Details tab holds no viewer lease. Clicking a sleeping card explicitly resumes and wakes it. A running computer streams a thumbnail. Opening the thumbnail expands the same session inside the app window.

Watching is the default. Pointer movement, clicks, keys, and scrolling stay local. Take over waits for a server acknowledgement before forwarding input. Focus loss releases held input. Closing the expanded viewer returns to watching. Leaving the Computer tab, closing the sidebar, switching bots, or closing the window drops the connection and its lease.

## Transport choice

The guest already uses Xvfb, x11vnc, and websockify. The API terminates VNC and streams PNG frames through an authenticated WebSocket. The native client paints frames with GPUI and sends typed input messages. The VNC password and private guest endpoint stay on the API host. Input and frame queues are bounded.

An embedded noVNC browser would reuse more guest UI, but the pinned GPUI WebView requires X11 on Linux and has native overlay restrictions. A native viewer keeps this app on Wayland and uses the same layout and controls as the rest of the application.

[Voxl](https://github.com/tembo/voxl) is a possible video transport for a later performance pass. It captures X11 and streams H.264 over WebSocket. Its web client decodes with WebCodecs. This application would need a native H.264 decoding path or an embedded browser, plus guest image changes and x264, libyuv, Opus, and PulseAudio dependencies. No Voxl performance claim has been independently measured here.

The chosen protocol reuses the installed desktop stack. It favors a smaller integration over video compression. The frame rate is capped and idle frames are not resent. Video playback and high motion can warrant a later transport change.
