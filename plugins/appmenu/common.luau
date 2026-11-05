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
-- Noctalia v5.4 panels have a fixed width per [[panel]] entry (only the
-- height can follow the content), so plugin.toml declares one panel per
-- width step, twice ("a"/"b" slots). Switching menus opens the *other*
-- slot: Noctalia then replaces the open panel instantly and anchors the new
-- one under the pointer, so the panel follows the hovered title without a
-- close/reopen gap (which would hand focus back to the app and flash).
M.PANEL_WIDTHS = { 180, 240, 300, 380, 480 }

function M.panelId(width, slot)
  return `goglioandrea/appmenu:menu_{width}_{slot}`
end

-- Room the host keeps around a panel's content (its own padding), in px.
local HOST_PADDING = 24
-- Average glyph advance of the panel's default text, in px. An estimate:
-- the "extra width" setting corrects it for wider or narrower fonts.
local CHAR_PX = 7.5
local GLYPH_COLUMN = 22 -- check/radio or chevron column, incl. gap
local NAV_BUTTON = 34

local function textWidth(text)
  local s = M.cleanLabel(text)
  return (utf8.len(s) or #s) * CHAR_PX
end

-- Width a menu needs, in px: its widest row at any depth (submenus open in
-- the same panel), plus the header and all paddings.
-- `childrenOf(node)` returns a node's children, lazily expanded ones included.
function M.estimateMenuWidth(node, childrenOf)
  local paddingH = M.intSetting("item_padding_h", 6, 0, 32)
  local widest = 0
  local function walk(n, depth)
    local kids = childrenOf(n)
    local toggles = false
    for _, item in ipairs(kids) do
      if type(item) == "table" and (item.toggle_type == "checkmark" or item.toggle_type == "radio") then
        toggles = true
      end
    end
    for _, item in ipairs(kids) do
      if type(item) == "table" and not M.isSeparator(item) then
        local w = textWidth(item.label) + 2 * paddingH
        if toggles then
          w += GLYPH_COLUMN
        end
        if M.isSubmenu(item) then
          w += GLYPH_COLUMN
          if depth < 4 then
            -- Inside a submenu the header is back button + its name.
            widest = math.max(widest, NAV_BUTTON + textWidth(item.label))
            walk(item, depth + 1)
          end
        end
        widest = math.max(widest, w)
      end
    end
  end
  walk(node, 1)
  if #childrenOf(node) == 0 then
    -- Not realised yet (lazy menu): leave room for what will arrive.
    widest = math.max(widest, 220)
  end
  local headerW = 0
  if noctalia.getConfig("show_menu_title") == true then
    headerW += textWidth(node.label)
  end
  if noctalia.getConfig("show_menu_arrows") == true then
    headerW += 2 * NAV_BUTTON
  end
  widest = math.max(widest, headerW)
  local panelPadding = M.intSetting("panel_padding", 0, 0, 32)
  local extra = M.intSetting("panel_extra_width", 0, -80, 200)
  return widest + 2 * panelPadding + HOST_PADDING + extra
end

-- The narrowest declared width step at least `width` wide (or the widest).
function M.widthStep(width)
  for _, w in ipairs(M.PANEL_WIDTHS) do
    if w >= width then
      return w
    end
  end
  return M.PANEL_WIDTHS[#M.PANEL_WIDTHS]
end

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
