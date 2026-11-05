--!nonstrict
-- AppMenu panel - the items of one top-level menu, with drill-down.
--
-- The bar says which top-level menu to show on "appmenu.open". A submenu
-- row pushes onto a navigation stack (back arrow to return); a leaf row
-- posts a click to the service and closes the panel. At the top level the
-- header arrows step to the neighbouring menus, like sliding along a
-- macOS menubar.
--
-- Firefox and GTK realise a submenu's items over AT-SPI only once it is
-- opened, so a submenu can arrive empty. The panel then asks the service to
-- expand it (`atspi-expand`) and renders the result from "appmenu.expanded".

local common = require("./common.luau")

local active = nil
local status = { state = "starting" }
local menuIndex = 1
local stack = {}
local expanded = {} -- [service .. "\n" .. path] = { children, failed }
local pending = {} -- same keys, expansion in flight
local hoveredKey = nil
local isOpen = false

local function tr(key, subst)
  return noctalia.tr(key, subst)
end

local function nodeKey(node)
  return `{node.service or ""}\n{node.path or ""}`
end

local function childrenOf(node)
  local own = common.children(node)
  if #own > 0 then
    return own
  end
  local cached = expanded[nodeKey(node)]
  if cached ~= nil then
    return cached.children
  end
  return own
end

local function topLevel()
  if active == nil or active.menu == nil then
    return {}
  end
  return common.children(active.menu)
end

-- Indices of the selectable top-level menus, for the header arrows.
local function topLevelIndices()
  local out = {}
  for i, item in ipairs(topLevel()) do
    if not common.isSeparator(item) and item.visible ~= false and common.cleanLabel(item.label) ~= "" then
      table.insert(out, i)
    end
  end
  return out
end

-- The node being shown, or nil when the menu changed under us.
local function currentNode()
  local node = topLevel()[menuIndex]
  if node == nil then
    return nil
  end
  for _, idx in ipairs(stack) do
    node = childrenOf(node)[idx]
    if node == nil then
      return nil
    end
  end
  return node
end

local function post(request)
  request.nonce = common.nonce()
  noctalia.state.set("appmenu.request", request)
end

local function needsExpand(node)
  if node == nil or node.service == common.SYNTHETIC_SERVICE then
    return false
  end
  if not common.isSubmenu(node) or #common.children(node) > 0 then
    return false
  end
  local key = nodeKey(node)
  return expanded[key] == nil and not pending[key]
end

local function requestExpand(node)
  if not needsExpand(node) or not common.validTarget(node.service, node.path) then
    return
  end
  pending[nodeKey(node)] = true
  post({
    kind = "expand",
    service = node.service,
    path = node.path,
    window = active ~= nil and active.window or "",
  })
end

local render

local function activate(item, index)
  if item.enabled == false then
    return
  end
  if common.isSubmenu(item) then
    table.insert(stack, index)
    hoveredKey = nil
    requestExpand(item)
    render()
    return
  end
  if not common.validTarget(item.service, item.path) then
    noctalia.log(`appmenu: refusing malformed menu target {item.service} {item.path}`)
    return
  end
  post({
    kind = "click",
    service = item.service,
    path = item.path,
    window = active ~= nil and active.window or "",
  })
  panel.close()
end

local function indicator(item)
  local checked = tonumber(item.toggle_state) == 1
  if item.toggle_type == "checkmark" and checked then
    return ui.glyph({ name = "check", size = 14, color = "primary" })
  elseif item.toggle_type == "radio" and checked then
    return ui.glyph({ name = "circle-dot", size = 14, color = "primary" })
  end
  return ui.box({ width = 14, height = 14 })
end

local function itemRow(item, index)
  local key = `item-{index}`
  local enabled = item.enabled ~= false
  local children = {
    indicator(item),
    ui.label({
      text = common.cleanLabel(item.label),
      flexGrow = 1,
      maxLines = 1,
      color = enabled and "on_surface" or "on_surface_variant",
    }),
  }
  if common.isSubmenu(item) then
    table.insert(children, ui.glyph({ name = "chevron-right", size = 14, color = "on_surface_variant" }))
  end
  return ui.row({
    key = key,
    gap = 8,
    paddingH = 10,
    paddingV = 6,
    radius = 6,
    align = "center",
    opacity = enabled and 1 or 0.5,
    fill = (enabled and hoveredKey == key) and "on_surface/0.08" or nil,
    onClick = enabled and function()
      activate(item, index)
    end or nil,
    onHover = function(state)
      local target = hoveredKey
      if state == "true" then
        target = key
      elseif hoveredKey == key then
        target = nil
      end
      if target ~= hoveredKey then
        hoveredKey = target
        render()
      end
    end,
  }, children)
end

local function messageRow(glyph, text, color)
  return ui.row({ gap = 10, align = "center", padding = 12 }, {
    ui.glyph({ name = glyph, size = 18, color = color or "on_surface_variant" }),
    ui.label({ text = text, color = "on_surface_variant", flexGrow = 1 }),
  })
end

local function stepMenu(delta)
  local indices = topLevelIndices()
  for pos, idx in ipairs(indices) do
    if idx == menuIndex then
      local nextIdx = indices[((pos - 1 + delta) % #indices) + 1]
      menuIndex = nextIdx
      stack = {}
      hoveredKey = nil
      noctalia.state.set("appmenu.panel", { open = true, index = menuIndex })
      render()
      return
    end
  end
end

local function header(node)
  local left
  if #stack > 0 then
    left = ui.button({
      glyph = "chevron-left",
      variant = "ghost",
      controlSize = "sm",
      tooltip = tr("panel.back"),
      onClick = function()
        table.remove(stack)
        hoveredKey = nil
        render()
      end,
    })
  else
    left = ui.button({
      glyph = "chevron-left",
      variant = "ghost",
      controlSize = "sm",
      tooltip = tr("panel.previous_menu"),
      onClick = function()
        stepMenu(-1)
      end,
    })
  end
  local items = {
    left,
    ui.label({
      text = common.cleanLabel(node.label),
      fontWeight = "bold",
      flexGrow = 1,
      maxLines = 1,
      textAlign = "center",
    }),
  }
  if #stack == 0 then
    table.insert(items, ui.button({
      glyph = "chevron-right",
      variant = "ghost",
      controlSize = "sm",
      tooltip = tr("panel.next_menu"),
      onClick = function()
        stepMenu(1)
      end,
    }))
  else
    -- Keep the title centred.
    table.insert(items, ui.box({ width = 32, height = 1 }))
  end
  return ui.row({ gap = 4, align = "center" }, items)
end

render = function()
  if not isOpen then
    return
  end

  if status.state == "missing" then
    panel.render(ui.column({ padding = 8 }, {
      messageRow("alert-triangle", tr("panel.missing", { bin = status.detail or "" }), "error"),
    }))
    return
  end

  local node = currentNode()
  if node == nil and #stack > 0 then
    -- The app rebuilt its menu; fall back to the top of this menu.
    stack = {}
    node = currentNode()
  end
  if node == nil then
    local text = active == nil and tr("panel.no_app") or tr("panel.no_menu")
    panel.render(ui.column({ padding = 8 }, { messageRow("menu-2", text) }))
    return
  end

  local rows = {}
  local kids = childrenOf(node)
  local key = nodeKey(node)
  if #kids == 0 then
    if pending[key] then
      table.insert(rows, messageRow("loader-2", tr("panel.loading")))
    elseif expanded[key] ~= nil and expanded[key].failed then
      table.insert(rows, messageRow("alert-triangle", tr("panel.expand_failed")))
    else
      table.insert(rows, messageRow("list", tr("panel.empty")))
    end
  end
  local lastWasSeparator = true
  for index, item in ipairs(kids) do
    if common.isSeparator(item) then
      if not lastWasSeparator then
        table.insert(rows, ui.separator({ key = `sep-{index}` }))
        lastWasSeparator = true
      end
    elseif item.visible ~= false and common.cleanLabel(item.label) ~= "" then
      table.insert(rows, itemRow(item, index))
      lastWasSeparator = false
    end
  end
  if lastWasSeparator and #rows > 1 and rows[#rows].type == "separator" then
    table.remove(rows)
  end

  panel.render(ui.column({ gap = 6, padding = 6, flexGrow = 1 }, {
    header(node),
    ui.separator({}),
    ui.scroll({ flexGrow = 1, gap = 1 }, rows),
  }))
end

local function showMenu(index)
  if type(index) ~= "number" then
    return
  end
  menuIndex = index
  stack = {}
  hoveredKey = nil
  requestExpand(topLevel()[menuIndex])
  render()
end

function onOpen(_context)
  isOpen = true
  local request = noctalia.state.get("appmenu.open")
  if type(request) == "table" then
    menuIndex = tonumber(request.index) or 1
  end
  stack = {}
  hoveredKey = nil
  noctalia.state.set("appmenu.panel", { open = true, index = menuIndex })
  requestExpand(topLevel()[menuIndex])
  render()
end

function onClose()
  isOpen = false
  stack = {}
  hoveredKey = nil
  -- Catch up with any focus change ignored while open.
  local latest = noctalia.state.get("appmenu.active")
  active = type(latest) == "table" and latest or nil
  noctalia.state.set("appmenu.panel", { open = false })
end

noctalia.state.watch("appmenu.open", function(request)
  if isOpen and type(request) == "table" then
    showMenu(tonumber(request.index))
    noctalia.state.set("appmenu.panel", { open = true, index = menuIndex })
  end
end)

noctalia.state.watch("appmenu.active", function(value)
  if isOpen and active ~= nil and common.isEmptySnapshot(value) then
    -- Focus left the app because the panel took the keyboard: keep showing
    -- (and clicking into) the menu the user opened.
    return
  end
  local previous = active
  active = type(value) == "table" and value or nil
  local sameApp = previous ~= nil and active ~= nil
    and previous.app_id == active.app_id
    and previous.window == active.window
  if not sameApp then
    -- Different app or window: everything cached belongs to the old one.
    expanded = {}
    pending = {}
    stack = {}
    menuIndex = 1
    if isOpen and active == nil then
      panel.close()
      return
    end
  end
  render()
end)

noctalia.state.watch("appmenu.expanded", function(result)
  if type(result) ~= "table" or type(result.service) ~= "string" or type(result.path) ~= "string" then
    return
  end
  local key = `{result.service}\n{result.path}`
  pending[key] = nil
  expanded[key] = {
    children = type(result.children) == "table" and result.children or {},
    failed = result.failed == true,
  }
  render()
end)

noctalia.state.watch("appmenu.status", function(value)
  if type(value) == "table" then
    status = value
    render()
  end
end)

active = noctalia.state.get("appmenu.active")
if type(active) ~= "table" then
  active = nil
end
status = noctalia.state.get("appmenu.status") or status
