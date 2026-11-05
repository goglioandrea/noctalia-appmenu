--!nonstrict
-- Helpers shared by the AppMenu entries (each entry loads its own copy).
--
-- State channels (noctalia.state, JSON-serialised, shared across entries):
--   "appmenu.active"   service -> UI   { app_id, title, window, compositor, source, menu }
--   "appmenu.status"   service -> UI   { state = "starting"|"running"|"missing"|"error", detail? }
--   "appmenu.open"     bar -> panel    { index, nonce }   which top-level menu to show
--   "appmenu.panel"    panel -> bar    { open = bool }
--   "appmenu.request"  UI -> service   { kind = "click"|"expand", service, path, window, nonce }
--   "appmenu.expanded" service -> panel { service, path, children }  lazily realised submenu

local M = {}

M.SYNTHETIC_SERVICE = "::synthetic"
M.PANEL_ID = "goglioandrea/appmenu:menu"

-- Fully transparent fill. Passing this (instead of dropping `fill`) matters:
-- the host keeps a container's previous fill when the prop disappears.
M.TRANSPARENT = "#00000000"

-- Fixed-width gap. A bare ui.box paints black, so give it a clear fill.
function M.spacer(width, height)
  return ui.box({ width = width, height = height or 1, fill = M.TRANSPARENT })
end

-- Integer setting clamped to [min, max], or `default` when unset.
function M.intSetting(key, default, min, max)
  local v = tonumber(noctalia.getConfig(key))
  if v == nil then
    return default
  end
  return math.clamp(math.floor(v), min, max)
end

-- A colour setting left empty means "use the default colour".
function M.colorSetting(key)
  local v = noctalia.getConfig(key)
  if type(v) == "string" and noctalia.string.trim(v) ~= "" then
    return v
  end
  return nil
end

-- A unique-enough token so two identical requests in a row still count as a
-- change for noctalia.state watchers.
local counter = 0
function M.nonce()
  counter += 1
  return `{noctalia.nowMs()}-{counter}`
end

-- Toolkits strip mnemonic markers ("&File") before exposing a label over
-- AT-SPI, so a literal "&" is real text and stays. Some fold the accelerator
-- into the name after a tab ("Open\tCtrl+O"); keep only the label part.
function M.cleanLabel(text)
  if type(text) ~= "string" then
    return ""
  end
  local s = text:gsub("\t.*$", "")
  return noctalia.string.trim(s)
end

-- Shorten `text` to `max` characters (UTF-8 aware) with an ellipsis.
function M.ellipsize(text, max)
  if type(text) ~= "string" then
    return ""
  end
  if utf8.len(text) == nil or utf8.len(text) <= max then
    return text
  end
  local cut = utf8.offset(text, max)
  return text:sub(1, (cut or (#text + 1)) - 1) .. "…"
end

function M.isSeparator(item)
  return type(item) == "table" and item.type == "separator"
end

function M.children(item)
  if type(item) == "table" and type(item.children) == "table" then
    return item.children
  end
  return {}
end

-- The bridge marks a node "submenu" when AT-SPI reports children or a MENU
-- role. Firefox/GTK realise a submenu's items only when it is opened, so a
-- submenu can arrive with no children: the panel asks the bridge to expand it.
function M.isSubmenu(item)
  return type(item) == "table" and (item.type == "submenu" or #M.children(item) > 0)
end

-- Values that go onto the bridge's command line. They are passed as argv
-- (no shell) after "--", so this is defence in depth against a malformed
-- snapshot rather than the only barrier.
function M.validTarget(service, path)
  if type(service) ~= "string" or type(path) ~= "string" then
    return false
  end
  if #service == 0 or #service > 255 or #path == 0 or #path > 1024 then
    return false
  end
  if service == M.SYNTHETIC_SERVICE then
    -- Synthetic items are dispatched by the bridge's own allow-list.
    return path:match("^[%w%-]+:[%w%-%.:_]+$") ~= nil
  end
  -- D-Bus bus name (":1.42" or "org.example.App") and object path.
  return service:match("^:?[%w_][%w_%-%.]*$") ~= nil and path:match("^/[%w_/]*$") ~= nil
end

function M.validWindow(window)
  return type(window) == "string" and #window <= 128 and not window:find("%c")
end

-- A snapshot with no focused app. The bridge publishes one when the
-- compositor reports no focused window - which can happen just because our
-- own panel took the keyboard.
function M.isEmptySnapshot(value)
  return type(value) ~= "table" or ((value.app_id or "") == "" and value.menu == nil)
end

function M.bridgePath()
  local configured = noctalia.getConfig("bridge_path")
  if type(configured) == "string" then
    configured = noctalia.string.trim(configured)
    if configured ~= "" then
      return configured
    end
  end
  return "noctalia-appmenu-bridge"
end

return M
