# Remote access

Run the server on a home machine or a VPS and connect the app from your laptop. The app needs the server's address and its owner key.

The owner key grants full access to your installation. Keep it private. Read it on the server with:

```sh
cat ~/.else/data/owner-token
```

## Tailscale

The server listens on `127.0.0.1:7878`, and also on the machine's Tailscale IPs when Tailscale is connected. It checks for new Tailscale addresses every 15 seconds. The installer prints the local and Tailscale URLs.

1. Connect Tailscale on the machine that runs the app.
2. In the app, paste the server's `http://100.x.y.z:7878` URL and the owner key. IPv6 Tailscale addresses work too.

Before it sends the key, the app checks that the IP belongs to a device on its tailnet.

## SSH tunnel

From the app machine, open a tunnel and keep it running:

```sh
ssh -N -L 7878:127.0.0.1:7878 you@server
```

In the app, choose **Edit configuration**, enter `127.0.0.1:7878`, paste the owner key, and choose **Connect to server**.

## Your own domain

Put an HTTPS reverse proxy in front of the server. It must forward HTTP and WebSocket requests to `127.0.0.1:7878` and rewrite the upstream `Host` header to that address.

The app requires HTTPS for every address except loopback and verified Tailscale IPs, and it checks the server certificate.

## Several servers

The app saves each server after it accepts the connection. Use the server switcher at the top of the sidebar to add servers and move between them. Email, Socials, MCP and Backups settings apply to the selected server.
