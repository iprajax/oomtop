# Agents ask before they load: MCP over stdio (replaying the recorded snapshot).
Output docs/media/mcp.gif
Set Width 1400
Set Height 640
Source docs/tapes/_setup.tape
Hide
Type `F=fixtures/macos/m5-air-agents.json; init='{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"demo","version":"1"}}}'; ask() { printf '%s\n' "$init" '{"jsonrpc":"2.0","method":"notifications/initialized"}' "{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/call\",\"params\":{\"name\":\"$1\",\"arguments\":$2}}" | oomtop --replay $F mcp | jq -c 'select(.id==2) | .result.structuredContent'; }; clear`
Enter
Show
Type `# an agent asks: can I load a 13 GB model?`
Enter
Type `ask can_fit '{"bytes":13000000000}'`
Enter
Sleep 4s
Type `ask suggest_reclaim '{}' | jq -c '.candidates[] | {label, gain_bytes}' 2>/dev/null || ask suggest_reclaim '{}'`
Enter
Sleep 4s
