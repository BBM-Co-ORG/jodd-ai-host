#!/bin/sh
# A RELEASE build of jodd-ai-mcp must ignore every JODD_AI_MCP_* override: the AI account could
# otherwise set one in its own shell rc and point the root-installed server at a policy it wrote
# itself. Debug builds honour them (the tests need that), so this checks both, the debug one to
# prove the policy below really enables claude where overrides apply.
# Run: sh test/release_overrides.sh RELEASE_BIN DEBUG_BIN
set -u
rel=${1:?usage: release_overrides.sh RELEASE_BIN DEBUG_BIN}
dbg=${2:?usage: release_overrides.sh RELEASE_BIN DEBUG_BIN}
t=$(mktemp -d); trap 'rm -rf "$t"' EXIT
fail=0

# An executable at an absolute path, and a policy that enables claude with it.
printf '#!/bin/sh\nexit 0\n' > "$t/claude"
chmod 755 "$t/claude"
cat > "$t/policy.toml" <<EOF
[harness.claude]
enabled = true
path = "$t/claude"
path_env = "/usr/bin:/bin"
timeout_secs = 120
EOF

status() {
	HOME=$t TMPDIR=$t SSH_ORIGINAL_COMMAND='jodd-ai-run 2 status' \
		JODD_AI_MCP_POLICY="$t/policy.toml" \
		JODD_AI_MCP_SLOTS_DIR="$t/slots" \
		JODD_AI_MCP_LOG_FILE="$t/calls.log" \
		"$1" --key 0123abcd
}

got=$(status "$dbg")
case $got in
*"preset=claude path=$t/claude"*) ;;
*) echo "FAIL: the debug build did not honour JODD_AI_MCP_POLICY, so this test proves nothing:"
   printf '%s\n' "$got"; fail=1 ;;
esac

got=$(status "$rel")
for p in claude codex; do
	if ! printf '%s\n' "$got" | grep -qx "preset=$p path=-"; then
		echo "FAIL: the release build reports preset=$p with a path; it honoured an override:"
		printf '%s\n' "$got"; fail=1
	fi
done

[ "$fail" = 0 ] && echo "release_overrides: ok"
exit "$fail"
