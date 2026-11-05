"""Rewrites pristine x11vnc 0.9.17 sources into the patched tree.

Usage: python3 -I make-patch.py PRISTINE_SRC_DIR OUT_SRC_DIR
Each edit must match exactly once, so drift in the source fails loudly.
"""
import pathlib
import sys

src, out = map(pathlib.Path, sys.argv[1:3])


def edit(name, pairs):
    text = (src / name).read_text(encoding="latin-1")
    for old, new in pairs:
        if text.count(old) != 1:
            sys.exit(f"{name}: expected one match for {old[:60]!r}, found {text.count(old)}")
        text = text.replace(old, new)
    (out / name).write_text(text, encoding="latin-1")


edit("keyboard.h", [(
    "extern int add_keysym(KeySym keysym);\n",
    "extern int add_keysym(KeySym keysym);\n"
    "extern void release_owner_input(rfbClientPtr client);\n",
)])

edit("connections.c", [(
    "\trfbLog(\"client_count: %d\\n\", client_count);\n\tlast_client_gone = dnow();\n",
    "\trfbLog(\"client_count: %d\\n\", client_count);\n\tlast_client_gone = dnow();\n"
    "\trelease_owner_input(client);\n",
), (
    "\tif (use_openssl && openssl_last_helper_pid) {\n\t\tcd->ssl_helper_pid = openssl_last_helper_pid;\n\t\topenssl_last_helper_pid = 0;\n\t}\n",
    "\tif (use_openssl && openssl_last_helper_pid) {\n\t\tcd->ssl_helper_pid = openssl_last_helper_pid;\n\t\topenssl_last_helper_pid = 0;\n\t}\n"
    "\t/* else.bot: -gone names the connection by its port, read now since\n\t * the socket is closed before that command runs */\n"
    "\tclient_set_net(client);\n",
)])

edit("keyboard.c", [
    (
        "static KeySym added_keysyms[0x100];\n",
        """static KeySym added_keysyms[0x100];

/*
 * else.bot: a keycode mapped for a press must reach the focused client
 * before the press, so add_keysym() waits for the server and then a
 * measured settle.  A full table recycles the least recently mapped
 * keycode that is not held, instead of ignoring the keystroke.  Presses
 * still queued when the viewer hangs up are dropped (each settle would
 * otherwise delay hand-back), while releases are still applied.
 */
#define KEYMAP_SETTLE_US 20000
static unsigned long added_seq[0x100];
static unsigned long add_counter;
static rfbClientPtr input_client;

static int peer_hung_up(rfbClientPtr client) {
	struct pollfd pfd;

	if (!client) {
		return 0;
	}
	if (client->sock < 0) {
		return 1;
	}
	pfd.fd = client->sock;
	pfd.events = POLLRDHUP;
	pfd.revents = 0;
	if (poll(&pfd, 1, 0) <= 0) {
		return 0;
	}
	return (pfd.revents & (POLLRDHUP|POLLHUP|POLLERR)) != 0;
}

static void note_dropped_press(rfbClientPtr client) {
	static rfbClientPtr noted;
	if (noted != client) {
		noted = client;
		fprintf(stderr, "x11vnc: viewer hung up, dropping its queued key presses\\n");
	}
}

static int added_keycode(KeySym keysym) {
	int n;
	for (n=0; n < 0x100; n++) {
		if (added_keysyms[n] != NoSymbol && added_keysyms[n] == keysym) {
			return n;
		}
	}
	return 0;
}

/* a cached lookup may still name a keycode since recycled for another keysym */
static KeyCode current_keycode(KeyCode kc, KeySym keysym) {
	if (kc != NoSymbol && added_keysyms[kc] != NoSymbol && added_keysyms[kc] != keysym) {
		return NoSymbol;
	}
	return kc;
}

static int recyclable_keycode(int minkey, int maxkey) {
	int kc, best = 0;
	for (kc = minkey+1; kc <= maxkey && kc < 0x100; kc++) {
		if (added_keysyms[kc] == NoSymbol || keycode_state[kc]) {
			continue;
		}
		if (!best || added_seq[kc] < added_seq[best]) {
			best = kc;
		}
	}
	return best;
}
""",
    ),
    (
        """	/* there can be a race before MappingNotify */
	for (n=0; n < 0x100; n++) {
		if (added_keysyms[n] == keysym) {
			return n;
		}
	}
""",
        """	/* there can be a race before MappingNotify */
	if ((n = added_keycode(keysym))) {
		return n;
	}
""",
    ),
    (
        """	for (kc = minkey+1; kc <= maxkey; kc++) {
		int i, j, didmsg = 0, is_empty = 1;
		char *str;
		KeySym newks[8];

		for (n=0; n < syms_per_keycode; n++) {
			if (keymap[ (kc-minkey) * syms_per_keycode + n]
			    != NoSymbol) {
				is_empty = 0;
				break;
			}
		}
		if (! is_empty) {
			continue;
		}
""",
        """	for (kc = minkey+1; kc <= maxkey + 1; kc++) {
		int i, j, didmsg = 0, is_empty = 1;
		char *str;
		KeySym newks[8];

		if (kc > maxkey) {
			kc = recyclable_keycode(minkey, maxkey);
			if (!kc) {
				fprintf(stderr, "x11vnc: keystroke ignored, every added keycode is held\\n");
				break;
			}
		} else {
			for (n=0; n < syms_per_keycode; n++) {
				if (keymap[ (kc-minkey) * syms_per_keycode + n]
				    != NoSymbol) {
					is_empty = 0;
					break;
				}
			}
			if (! is_empty) {
				continue;
			}
		}
""",
    ),
    (
        """		XFlush_wr(dpy);
		added_keysyms[kc] = keysym;
		ret = kc;
		break;
	}
	XFree_wr(keymap);
	return ret;
""",
        """		XSync(dpy, False);
		usleep(KEYMAP_SETTLE_US);
		added_keysyms[kc] = keysym;
		added_seq[kc] = ++add_counter;
		ret = kc;
		break;
	}
	XFree_wr(keymap);
	if (ret && peer_hung_up(input_client)) {
		note_dropped_press(input_client);
		return 0;
	}
	return ret;
""",
    ),
    (
        """			if (keysym != xkbkeysyms[kc][grp][lvl]) {
				continue;
			}
""",
        """			if (keysym != xkbkeysyms[kc][grp][lvl]) {
				continue;
			}
			if (current_keycode(kc, keysym) == NoSymbol) {
				continue;
			}
""",
    ),
    (
        """	if (!found && add_keysyms && keysym && ! IsModifierKey(keysym)) {
		int new_kc = add_keysym(keysym);
""",
        """	if (!found && add_keysyms && keysym && ! IsModifierKey(keysym)) {
		int new_kc = down ? add_keysym(keysym) : added_keycode(keysym);
""",
    ),
    (
        """		k = XKeysymToKeycode(dpy, (KeySym) keysym);
		X_UNLOCK;
	}
	if (k == NoSymbol && add_keysyms && ! IsModifierKey(keysym)) {
		int new_kc = add_keysym(keysym);
""",
        """		k = XKeysymToKeycode(dpy, (KeySym) keysym);
		X_UNLOCK;
	}
	k = current_keycode(k, keysym);
	if (k == NoSymbol && add_keysyms && ! IsModifierKey(keysym)) {
		int new_kc = down ? add_keysym(keysym) : added_keycode(keysym);
""",
    ),
    (
        """	k = XKeysymToKeycode(dpy, (KeySym) keysym);

	if (k == NoSymbol && add_keysyms && ! IsModifierKey(keysym)) {
		int new_kc = add_keysym(keysym);
""",
        """	k = current_keycode(XKeysymToKeycode(dpy, (KeySym) keysym), keysym);

	if (k == NoSymbol && add_keysyms && ! IsModifierKey(keysym)) {
		int new_kc = down ? add_keysym(keysym) : added_keycode(keysym);
""",
    ),
    (
        """	if (threads_drop_input) {
		return;
	}

	dtime0(&tnow);
	got_keyboard_calls++;
""",
        """	if (threads_drop_input) {
		return;
	}
	if (down && peer_hung_up(client)) {
		note_dropped_press(client);
		return;
	}
	input_client = client;

	dtime0(&tnow);
	got_keyboard_calls++;
""",
    ),
    (
        "static rfbClientPtr last_keyboard_client = NULL;\n",
        """static rfbClientPtr last_keyboard_client = NULL;

/*
 * else.bot: when a viewer goes, release what x11vnc still holds for it,
 * since its queued releases may never have arrived, then wait for the
 * server, so the -gone command runs only once every event is applied.
 */
void release_owner_input(rfbClientPtr client) {
	int k, b;

	if (input_client == client) {
		input_client = NULL;
	}
	if (view_only) {
		return;
	}
	RAWFB_RET_VOID
#if !NO_X11
	X_LOCK;
	if (client == last_keyboard_client || client == last_pointer_client) {
		last_keyboard_client = NULL;
		last_pointer_client = NULL;
		mod_state = 0;
		for (k = 0; simple_mods[k] != NoSymbol; k++) {
			track_mod_state(simple_mods[k], False, True);
		}
		for (k = 0; k < 0x100; k++) {
			if (keycode_state[k]) {
				XTestFakeKeyEvent_wr(dpy, -1, (KeyCode) k, False, CurrentTime);
			}
		}
		for (b = 1; b <= MAX_BUTTONS; b++) {
			if (button_mask & (1 << (b-1))) {
				do_button_mask_change(0, b, NULL);
			}
		}
		button_mask = 0;
	}
	XSync(dpy, False);
	X_UNLOCK;
#endif
}
""",
    ),
    (
        '#include "x11vnc.h"\n',
        '#include "x11vnc.h"\n#include <poll.h>\n#ifndef POLLRDHUP\n#define POLLRDHUP 0x2000\n#endif\n',
    ),
])
