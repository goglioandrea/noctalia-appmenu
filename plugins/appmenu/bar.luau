--!nonstrict
-- AppMenu bar widget - the focused app's name and its top-level menu titles.
--
-- Clicking a title opens the panel on that menu; clicking the open title
-- again closes it, and clicking another title while the panel is open
-- switches the panel to that menu. On a vertical bar there is no room for
-- titles, so the widget is a single glyph that opens the first menu.

local common = require("./common.luau")

local active = nil
local status = { state = "starting" }
local panelOpen = false
local openIndex = nil
local hovered = nil

local function topLevelMenus()
  local menus = {}
  if active == nil or active.menu == nil then
    return menus
  end
  for index, item in ipairs(common.children(active.menu)) do
    if not common.isSeparator(item) and item.visible ~= false then
      local label = common.cleanLabel(item.label)
      if label ~= "" then
        table.insert(menus, { index = index, label = label, enabled = item.enabled ~= false })
      end
    end
  end
  return menus
end

local function appName()
  if active == nil then
    return ""
  end
  -- The menu root carries the app's display name when the bridge built a
  -- fallback menu; otherwise derive one from the app id.
  local root = active.menu
  if root ~= nil and type(root.label) == "string" and root.label ~= "" and active.source ~= "atspi" then
    return common.cleanLabel(root.label)
  end
  local id = active.app_id or ""
  local short = id:match("([^%.]+)$") or id
  if short == "" then
    return ""
  end
  return short:sub(1, 1):upper() .. short:sub(2)
end

local function openMenu(index)
  if panelOpen and openIndex == index then
    openIndex = nil
    noctalia.togglePanel(common.PANEL_ID)
    return
  end
  openIndex = index
  noctalia.state.set("appmenu.open", { index = index, nonce = common.nonce() })
  if not panelOpen then
    noctalia.togglePanel(common.PANEL_ID)
  end
end

local function tooltipFor()
  if status.state == "missing" then
    return noctalia.tr("tooltip.missing", { bin = status.detail or "" })
  elseif status.state == "error" then
    return status.detail or noctalia.tr("status.exited")
  elseif active == nil then
    return noctalia.tr("tooltip.waiting")
  end
  return nil
end

local render

local intSetting = common.intSetting
local colorSetting = common.colorSetting

local function titleButton(menu, opts)
  local selected = panelOpen and openIndex == menu.index
  local isHovered = hovered == menu.index
  local showFill = (opts.highlight == "open" and selected)
    or (opts.highlight == "hover" and (selected or isHovered))
  -- Always pass a fill: the host keeps the previous one when the prop is
  -- dropped, which left highlights stuck behind every title once hovered.
  local fill = common.TRANSPARENT
  if showFill then
    fill = selected and "primary/0.22" or "on_surface/0.08"
  end
  local color = opts.color
  if selected and opts.highlight ~= "never" and opts.selectedColor ~= nil then
    color = opts.selectedColor
  end
  return ui.row({
    key = `menu-{menu.index}`,
    paddingH = opts.padding,
    paddingV = 2,
    radius = 6,
    align = "center",
    fill = fill,
    opacity = menu.enabled and 1 or 0.5,
    onClick = function()
      openMenu(menu.index)
    end,
    onHover = function(state)
      if opts.highlight ~= "hover" then
        return
      end
      local target = hovered
      if state == "true" then
        target = menu.index
      elseif hovered == menu.index then
        target = nil
      end
      if target ~= hovered then
        hovered = target
        render()
      end
    end,
  }, {
    ui.label({
      text = common.ellipsize(menu.label, opts.maxChars),
      color = color,
      fontWeight = menu.bold and "bold" or nil,
    }),
  })
end

render = function()
  local menus = topLevelMenus()
  local tip = tooltipFor()
  if tip ~= nil then
    barWidget.setTooltip(tip)
  else
    barWidget.clearTooltip()
  end

  if #menus == 0 and active == nil and noctalia.getConfig("hide_when_empty") == true then
    barWidget.setVisible(false)
    return
  end
  barWidget.setVisible(true)

  if barWidget.isVertical() then
    barWidget.render(ui.column({ align = "center" }, {
      ui.glyph({ name = status.state == "missing" and "alert-triangle" or "menu-2" }),
    }))
    return
  end

  local opts = {
    maxChars = intSetting("max_title_chars", 24, 4, 80),
    padding = intSetting("menu_padding", 6, 0, 24),
    highlight = noctalia.getConfig("menu_highlight") or "open",
    color = colorSetting("menu_text_color"),
    selectedColor = colorSetting("menu_selected_text_color"),
  }
  local appGap = intSetting("app_name_spacing", 6, 0, 48)
  local menuGap = intSetting("menu_spacing", 0, 0, 48)

  local children = {}
  local function spacer(width)
    if width > 0 then
      table.insert(children, common.spacer(width))
    end
  end

  if status.state == "missing" or (status.state == "error" and active == nil) then
    table.insert(children, ui.glyph({ name = "alert-triangle", color = "error" }))
    spacer(appGap)
  end

  local name = appName()
  local showName = noctalia.getConfig("show_app_name") ~= false and name ~= ""
  -- Fallback menus open with a menu named after the app: show that one as
  -- the bold app name instead of printing the name twice.
  if showName and #menus > 0 and menus[1].label == name then
    menus[1].bold = true
    showName = false
  end
  if showName then
    table.insert(children, ui.label({
      text = common.ellipsize(name, 28),
      fontWeight = "bold",
      color = colorSetting("app_name_color"),
    }))
    -- The titles carry their own horizontal padding; subtract it so the
    -- setting is the visible gap between the name and the first title.
    spacer(math.max(0, appGap - opts.padding))
  end

  local maxMenus = intSetting("max_menus", 12, 1, 30)
  for i, menu in ipairs(menus) do
    if i > maxMenus then
      break
    end
    if i > 1 then
      spacer(menuGap)
    end
    table.insert(children, titleButton(menu, opts))
  end

  if #children == 0 then
    -- Something must stay clickable/hoverable so the tooltip can explain.
    table.insert(children, ui.glyph({ name = "menu-2", color = "on_surface_variant" }))
  end

  barWidget.render(ui.row({ gap = 0, align = "center" }, children))
end

-- Clicks on the capsule outside a title (app name, padding) open the first
-- menu, like clicking the app name on macOS.
function onClick()
  local menus = topLevelMenus()
  if #menus > 0 then
    openMenu(menus[1].index)
  end
end

noctalia.state.watch("appmenu.active", function(value)
  if panelOpen and active ~= nil and common.isEmptySnapshot(value) then
    -- See panel.luau: the panel holding the keyboard is not a focus change.
    return
  end
  local previousApp = active and active.app_id or nil
  active = type(value) == "table" and value or nil
  if active == nil or active.app_id ~= previousApp then
    hovered = nil
  end
  render()
end)

noctalia.state.watch("appmenu.status", function(value)
  if type(value) == "table" then
    status = value
    render()
  end
end)

noctalia.state.watch("appmenu.panel", function(value)
  panelOpen = type(value) == "table" and value.open == true
  if type(value) == "table" and type(value.index) == "number" then
    openIndex = value.index
  end
  if not panelOpen then
    openIndex = nil
    -- Catch up with any focus change ignored while the panel was open.
    local latest = noctalia.state.get("appmenu.active")
    active = type(latest) == "table" and latest or nil
  end
  render()
end)

active = noctalia.state.get("appmenu.active")
if type(active) ~= "table" then
  active = nil
end
status = noctalia.state.get("appmenu.status") or status
render()
