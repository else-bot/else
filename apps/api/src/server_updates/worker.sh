#!/usr/bin/env bash
set -uo pipefail
umask 077
home="$1"
version="$2"
job="$3"
exec >>"$job/output.log" 2>&1
printf '%s\n' "$$" >"$job/pid"
finish() {
	local status=$?
	printf '%s\n' "$status" >"$job/result.tmp"
	mv "$job/result.tmp" "$job/result"
}
trap finish EXIT
while IFS= read -r name; do
	case "$name" in ELSE_* | SECRET_KEY* | DATABASE_URL | OWNER_TOKEN_FILE) unset "$name" ;; esac
done < <(compgen -e)
export ELSE_HOME="$home" GIT_TERMINAL_PROMPT=0
"$home/bin/elsebot" update --yes --server-only --version "$version" || exit $?
pointer="$home/server-current"
[ -e "$pointer" ] || pointer="$home/current"
installed="$("$pointer/bin/else-api" --version)" || exit $?
[ "$installed" = "else-api $version" ] || {
	printf 'The installer finished, but the server does not report %s.\n' "$version"
	exit 1
}
