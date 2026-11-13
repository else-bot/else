#!/usr/bin/env bash
# Usage from repository root: shoot.sh BOX OUTDIR [--no-build]
# Only the sandbox is mutated. Layout coordinates below are logical px at 1440x900.
set -euo pipefail
BOX=${1:?box required}
H=$(dirname "$(readlink -f "$0")")
ROOT=${ROOT:-$(dirname "$(dirname "$H")")}
STATE=${HARNESS_STATE_DIR:-$ROOT/.else-dev/visual-desktop}
OUT=${2:-$STATE/shots/r0}
mkdir -p "$OUT"
printf "Capture incomplete\n" > "$OUT/.incomplete"
if [[ ${3:-} != --no-build ]]; then
  # Build outputs stay in the harness too. Existing caller-owned builds use --no-build.
  (cd "$ROOT" && CARGO_BUILD_JOBS=8 cargo build --locked -q --workspace --bin else-desktop \
    --target-dir "$STATE/build" --config build.incremental=false)
  export HARNESS_DESKTOP_BINARY="$STATE/build/debug/else-desktop"
fi
BHOME=$(omabox -b "$BOX" path --home)
if [[ -n ${HARNESS_DESKTOP_BINARY:-} ]]; then
  command cp -f "$HARNESS_DESKTOP_BINARY" "$BHOME/else-desktop-harness.new"
  command mv -f "$BHOME/else-desktop-harness.new" "$BHOME/else-desktop-harness"
  HARNESS_DESKTOP_BINARY=/home/sbx/else-desktop-harness
fi
command cp -f "$H/relaunch.py" "$BHOME/relaunch.py"
omabox -b "$BOX" dnd on
omabox -b "$BOX" dnd --clear
omabox -b "$BOX" pointer hide
W=class:else-bot
settle() {
  # Only the running bot's sidebar animation is masked. All captured content must settle.
  omabox -b "$BOX" wait --timeout 5s still --window "$W" --ignore '0,0 238x850' >/dev/null
}
keys() {
  local code=0
  omabox -b "$BOX" keys --window "$W" --wait --timeout 3s --ignore '0,0 238x850' "$@" >/dev/null 2>&1 || code=$?
  # 124 means input landed but no visual change; never resend it.
  [[ $code == 0 || $code == 124 ]] || return "$code"
}
click() { omabox -b "$BOX" click --window "$W" "$@" >/dev/null; settle; }
scroll() { omabox -b "$BOX" scroll --window "$W" "$1" "$2" "$3" --source wheel >/dev/null; settle; }
shot() {
  omabox -b "$BOX" pointer --window "$W" -- move 900 35 >/dev/null
  settle
  local png
  png=$(omabox -b "$BOX" shot --window "$W")
  command cp -f "$png" "$OUT/$1.png"
  printf '%s\n' "$1.png"
}
launch() {
  local token
  token=$(python3 -c 'import uuid; print(uuid.uuid4())')
  omabox -b "$BOX" run -d -- python3 /home/sbx/relaunch.py "$token" "${HARNESS_DESKTOP_BINARY:-}" >/dev/null
  # Hyprland may reuse an address immediately, so readiness is tied to the new PID.
  omabox -b "$BOX" wait --timeout 30s until -- python3 /home/sbx/relaunch.py --check "$token" >/dev/null
  omabox -b "$BOX" wait --timeout 30s window "$W" >/dev/null
  omabox -b "$BOX" window --window "$W" --size 1440x900 >/dev/null
  settle
}
theme() { omabox -b "$BOX" run -- omarchy-theme-set "$1" >"$OUT/theme-$1.log" 2>&1; }
select_bot() {
  # The pinned main row has no accessibility node. This anchor also restores
  # GPUI app focus after a modal closes. Remaining navigation uses sidebar keys.
  click 130 100
  keys Escape
  case $1 in
    main) ;;
    code) keys Down ;;
    inbox) keys Down Down ;;
    research) keys Down Down Down ;;
    travel) keys Down Down Down Down ;;
    empty) keys Down Down Down Down Down ;;
    working) keys Down Down Down Down Down Down ;;
    *) return 2 ;;
  esac
}
main_bottom() { scroll 850 620 4500; }
click_named() {
  local point
  # GPUI reports physical screenshot pixels; omabox --find currently treats
  # them as logical pixels at scale 2. Convert the tree bounds explicitly.
  point=$(omabox -b "$BOX" tree --window "$W" --all --find "$1" | python3 "$H/tree_point.py" "$1" "${TREE_SCALE:-2}") || return 1
  local x y
  read -r x y <<< "$point"
  click "$x" "$y"
}
pick_purple() {
  local point x y
  point=$(omabox -b "$BOX" tree --window "$W" --all --find 'House shape' | python3 "$H/tree_point.py" 'House shape' "${TREE_SCALE:-2}")
  read -r x y <<< "$point"
  # Both are column four; the narrower color cells shift their center 2 px left.
  # From the House center: 17 px to its edge, 12 gap, 30 first row, 4 gap, 15 center.
  click "$((x - 2))" "$((y + 78))"
}
settings_page() {
  local label=$1 fallback_y=$2
  # Most nav items have accessible names; Backups currently lacks one.
  if ! click_named "$label"; then
    # Settings nav is at x=340; y comes from its row in the 1440x900 sheet.
    click 340 "$fallback_y"
  fi
}

# Fixed 24-image council set: twenty dark screens, four light screens.
theme cursor-dark
launch
select_bot main
main_bottom
shot dark-01-release-plan
# Scroll back from the bottom to show the delegation group and saved status cards.
scroll 850 450 -195
shot dark-02-bot-activity
# The rules card opens the versioned diff; coordinate is calibrated below.
# Capture it separately so the council sees the actual diff component.
select_bot code
shot dark-03-approval-and-failure
select_bot inbox
shot dark-04-email-and-notice
select_bot research
shot dark-05-owner-choices
select_bot travel
shot dark-06-secret-request
select_bot working
# The main working pulse animates too; keep its screenshot instant and deliberate.
omabox -b "$BOX" wait --timeout 3s still --window "$W" --ignore '0,0 238x850' --ignore '240,30 1200x740' >/dev/null
png=$(omabox -b "$BOX" shot --window "$W")
command cp -f "$png" "$OUT/dark-07-working.png"
select_bot main
main_bottom
keys ctrl+shift+b
# Details tab at x=1188,y=58; the tabs are the panel header. GPUI does not expose it to AT-SPI.
click 1188 58
scroll 1300 450 -4500
shot dark-08-bot-details
scroll 1300 650 4500
shot dark-09-schedules-and-triggers
# Computer tab at x=1327,y=58; the two tabs share the header beside the hide button.
click 1327 58
shot dark-10-computer
keys ctrl+shift+b
select_bot empty
shot dark-11-empty-conversation
keys ctrl+shift+n
# The dialog opens with Bot name focused. Fill it without creating a bot.
keys -t 'Calendar Assistant'
click_named 'House shape'
# GPUI omits color nodes; anchor the swatch to the accessible shape grid.
pick_purple
shot dark-12-create-bot
keys Escape
# Shell-level settings shortcut survives focus restoration after a modal.
keys ctrl+comma
settings_page Servers 265
shot dark-13-settings-servers
settings_page 'App updates' 301
shot dark-14-settings-updates
settings_page Email 392
shot dark-15-settings-email
settings_page Socials 428
shot dark-16-settings-socials
settings_page MCP 464
shot dark-17-settings-mcp
settings_page Skills 500
shot dark-18-settings-skills
settings_page Backups 536
shot dark-19-settings-backups
keys Escape
# One additional dark shot opens the saved rules version from the Details pane.
select_bot main
keys ctrl+shift+b
click 1188 58
scroll 1300 450 -4500
# Rules summary row at x=1260,y=292 in the default Details layout.
click 1260 292
click_named History
shot dark-20-rules-diff
keys Escape

theme catppuccin-latte
launch
select_bot main
main_bottom
shot light-01-release-plan
keys ctrl+shift+b
click 1188 58
scroll 1300 450 -4500
shot light-02-bot-details
keys ctrl+shift+b
keys ctrl+shift+n
keys -t 'Calendar Assistant'
click_named 'House shape'
# GPUI omits color nodes; anchor the swatch to the accessible shape grid.
pick_purple
shot light-03-create-bot
keys Escape
keys ctrl+comma
settings_page Servers 265
shot light-04-settings-servers
keys Escape
theme cursor-dark
command rm -f "$OUT/.incomplete"
printf 'Captured %s PNG files in %s\n' "$(find "$OUT" -maxdepth 1 -name '*.png' | wc -l)" "$OUT"
