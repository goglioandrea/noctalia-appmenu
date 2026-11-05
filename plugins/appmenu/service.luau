--!nonstrict
-- AppMenu service - the only entry that talks to noctalia-appmenu-bridge.
--
-- The bridge follows compositor focus (niri or Umbriel), walks the focused
-- app's menubar over AT-SPI, and in `--stream` mode prints one JSON line per
-- change on stdout:
--
--   {"event":"active","seq":N,"data":{...}}   snapshot inline (fits 64 KiB)
--   {"event":"active","seq":N,"file":"..."}   too big: read active.json
--   {"event":"heartbeat"}                      every 5 s
--
-- runStream has no exit callback, so the command prints a sentinel line when
-- the bridge exits; update() then restarts it with a backoff. Streams are
-- killed (whole process group) when the plugin reloads or is disabled.

local common = require("./common.luau")

local UPDATE_INTERVAL_MS = 1000
local BACKOFF_MIN_MS = 2000
local BACKOFF_MAX_MS = 60000
-- A run that lasted this long was healthy: the next restart starts fast.
local HEALTHY_RUN_MS = 60000
-- No line (not even a heartbeat) for this long: report the bridge as stuck.
local STALE_MS = 20000
local CLICK_TIMEOUT_MS = 15000
local EXIT_SENTINEL = "appmenu-bridge-exited"

local streaming = false
local startedAtMs = 0
local lastLineMs = 0
local nextStartMs = 0
local backoffMs = BACKOFF_MIN_MS
local status = { state = "starting" }
local lastSeq = -1

local function publishStatus(state, detail)
  if status.state == state and status.detail == detail then
    return
  end
  status = { state = state, detail = detail }
  noctalia.state.set("appmenu.status", status)
end

-- POSIX single-quote a string for /bin/sh.
local function shellQuote(s)
  return "'" .. s:gsub("'", "'\\''") .. "'"
end

local function streamCommand(bin)
  -- Bridge logs go to a per-session file in the private runtime dir
  -- (truncated on every start, so it cannot grow without bound).
  local runtime = noctalia.getenv("XDG_RUNTIME_DIR")
  local logRedirect = "2>/dev/null"
  if runtime ~= nil and runtime ~= "" and runtime:sub(1, 1) == "/" then
    logRedirect = "2>" .. shellQuote(runtime .. "/noctalia-appmenu-bridge.log")
  end
  -- Plain concatenation: the JSON braces must not be read as interpolation.
  return shellQuote(bin)
    .. " --stream "
    .. logRedirect
    .. "; printf '\\n{\"event\":\""
    .. EXIT_SENTINEL
    .. "\"}\\n'"
end

-- Turn the bridge's active.json payload into the snapshot the UI renders.
local function publishActive(payload)
  if type(payload) ~= "table" or payload.v ~= 1 then
    return
  end
  local window = payload.focus_window
  if type(window) ~= "string" or window == "" then
    -- Older bridges only had the numeric niri id.
    local winid = tonumber(payload.focus_winid)
    window = (winid ~= nil and winid > 0) and tostring(math.floor(winid)) or ""
  end
  local menu = payload.menu
  if type(menu) ~= "table" then
    menu = nil
  end
  noctalia.state.set("appmenu.active", {
    app_id = type(payload.app_id) == "string" and payload.app_id or "",
    title = type(payload.title) == "string" and payload.title or "",
    window = common.validWindow(window) and window or "",
    compositor = type(payload.compositor) == "string" and payload.compositor or "",
    source = type(payload.source) == "string" and payload.source or "",
    menu = menu,
  })
end

-- Only ever read the bridge's own state file, never a path chosen elsewhere.
local function isActiveJsonPath(path)
  return type(path) == "string"
    and path:sub(1, 1) == "/"
    and not path:find("/%.%./")
    and path:match("/noctalia%-appmenu/active%.json$") ~= nil
end

local function onLine(line)
  lastLineMs = noctalia.nowMs()
  local decoded = noctalia.json.decode(line)
  if type(decoded) ~= "table" then
    return
  end
  local event = decoded.event
  if event == EXIT_SENTINEL then
    streaming = false
    local ranMs = noctalia.nowMs() - startedAtMs
    if ranMs >= HEALTHY_RUN_MS then
      backoffMs = BACKOFF_MIN_MS
    end
    nextStartMs = noctalia.nowMs() + backoffMs
    backoffMs = math.min(backoffMs * 2, BACKOFF_MAX_MS)
    publishStatus("error", noctalia.tr("status.exited"))
    noctalia.state.set("appmenu.active", nil)
    return
  end
  if event == "heartbeat" then
    publishStatus("running", nil)
    return
  end
  if event ~= "active" then
    return
  end
  publishStatus("running", nil)
  local seq = tonumber(decoded.seq) or 0
  if type(decoded.data) == "table" then
    lastSeq = seq
    publishActive(decoded.data)
  elseif isActiveJsonPath(decoded.file) then
    noctalia.readFileAsync(decoded.file, function(contents, err)
      -- A newer inline snapshot may have landed while the file was read.
      if seq < lastSeq then
        return
      end
      if contents == nil then
        noctalia.log(`appmenu: reading {decoded.file} failed: {err}`)
        return
      end
      lastSeq = seq
      publishActive(noctalia.json.decode(contents))
    end)
  end
end

local function startBridge()
  if streaming or noctalia.nowMs() < nextStartMs then
    return
  end
  local bin = common.bridgePath()
  -- A bare name must resolve on PATH; an absolute path is taken as given.
  if bin:sub(1, 1) ~= "/" and not noctalia.commandExists(bin) then
    publishStatus("missing", bin)
    nextStartMs = noctalia.nowMs() + BACKOFF_MAX_MS
    return
  end
  if bin:sub(1, 1) == "/" and not noctalia.fileExists(bin) then
    publishStatus("missing", bin)
    nextStartMs = noctalia.nowMs() + BACKOFF_MAX_MS
    return
  end
  streaming = noctalia.runStream(streamCommand(bin), onLine)
  if streaming then
    startedAtMs = noctalia.nowMs()
    lastLineMs = startedAtMs
    publishStatus("starting", nil)
  else
    nextStartMs = noctalia.nowMs() + backoffMs
    publishStatus("error", noctalia.tr("status.spawn_failed"))
  end
end

-- argv for a one-shot bridge subcommand. `--window=` keeps an id that starts
-- with "-" from being read as a flag; "--" ends options before the
-- app-supplied service/path.
local function subcommandArgv(sub, request)
  local window = common.validWindow(request.window) and request.window or ""
  return { common.bridgePath(), sub, `--window={window}`, "--", request.service, request.path }
end

local function runClick(request)
  noctalia.runAsync(subcommandArgv("atspi-click", request), function(result)
    if result.exitCode == 0 then
      return
    end
    -- Exit 2: the item vanished (the app rebuilt its menu). The bridge has
    -- already been asked to re-walk; the fresh menu arrives on the stream.
    if result.exitCode == 2 then
      return
    end
    local detail = noctalia.string.trim(result.stderr or "")
    if #detail > 300 then
      detail = detail:sub(-300)
    end
    noctalia.notifyError(noctalia.tr("notify.click_failed"), detail)
  end, CLICK_TIMEOUT_MS)
end

local function runExpand(request)
  noctalia.runAsync(subcommandArgv("atspi-expand", request), function(result)
    local children = nil
    if result.exitCode == 0 then
      local decoded = noctalia.json.decode(result.stdout or "")
      if type(decoded) == "table" then
        children = decoded
      end
    end
    noctalia.state.set("appmenu.expanded", {
      service = request.service,
      path = request.path,
      children = children or {},
      failed = children == nil,
      nonce = common.nonce(),
    })
  end, CLICK_TIMEOUT_MS)
end

noctalia.state.watch("appmenu.request", function(request)
  if type(request) ~= "table" or not common.validTarget(request.service, request.path) then
    return
  end
  if request.kind == "click" then
    runClick(request)
  elseif request.kind == "expand" then
    runExpand(request)
  end
end)

function update()
  if streaming and noctalia.nowMs() - lastLineMs > STALE_MS then
    publishStatus("error", noctalia.tr("status.stale"))
  end
  startBridge()
end

function onExit(_signal, _reason)
  -- The host kills the bridge's process group with the stream; just clear
  -- what the UI shows.
  noctalia.state.set("appmenu.active", nil)
end

noctalia.setUpdateInterval(UPDATE_INTERVAL_MS)
publishStatus("starting", nil)
startBridge()
