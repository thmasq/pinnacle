-- This Source Code Form is subject to the terms of the Mozilla Public
-- License, v. 2.0. If a copy of the MPL was not distributed with this
-- file, You can obtain one at https://mozilla.org/MPL/2.0/.

local log = require("pinnacle.log")
local client = require("pinnacle.grpc.client").client
local window_v0alpha1 = require("pinnacle.grpc.defs").pinnacle.window.v0alpha1
local window_service = require("pinnacle.grpc.defs").pinnacle.window.v0alpha1.WindowService

local set_or_toggle = {
    SET = require("pinnacle.grpc.defs").pinnacle.v0alpha1.SetOrToggle.SET_OR_TOGGLE_SET,
    [true] = require("pinnacle.grpc.defs").pinnacle.v0alpha1.SetOrToggle.SET_OR_TOGGLE_SET,
    UNSET = require("pinnacle.grpc.defs").pinnacle.v0alpha1.SetOrToggle.SET_OR_TOGGLE_UNSET,
    [false] = require("pinnacle.grpc.defs").pinnacle.v0alpha1.SetOrToggle.SET_OR_TOGGLE_UNSET,
    TOGGLE = require("pinnacle.grpc.defs").pinnacle.v0alpha1.SetOrToggle.SET_OR_TOGGLE_TOGGLE,
}

---@lcat nodoc
---@class WindowHandleModule
local window_handle = {}

---A window handle.
---
---This is a handle to an application window that allows manipulation of the window.
---
---If the window is destroyed, the handle will become invalid and may not do
---what you want it to.
---
---You can retrieve window handles through the various `get` functions in the `Window` module.
---@class WindowHandle
---@field id integer
local WindowHandle = {}

---Window management.
---
---This module helps you deal with setting windows to fullscreen and maximized, setting their size,
---moving them between tags, and various other actions.
---@class Window
---@lcat nodoc
---@field private handle WindowHandleModule
local window = {}
window.handle = window_handle

---Get all windows.
---
---#### Example
---```lua
---local windows = Window.get_all()
---for _, window in ipairs(windows) do
---    print(window:props().class)
---end
---```
---@return WindowHandle[] windows Handles to all windows
function window.get_all()
    local response, err = client:unary_request(window_service.Get, {})

    if err then
        log:error(err)
        return {}
    end

    ---@cast response pinnacle.window.v0alpha1.GetResponse

    local handles = window_handle.new_from_table(response.window_ids or {})

    return handles
end

---Get the currently focused window.
---
---#### Example
---```lua
---local focused = Window.get_focused()
---if focused then
---    print(focused:props().class)
---end
---```
---@return WindowHandle | nil window A handle to the currently focused window
function window.get_focused()
    local handles = window.get_all()

    ---@type (fun(): WindowProperties)[]
    local requests = {}

    for i, handle in ipairs(handles) do
        requests[i] = function()
            return handle:props()
        end
    end

    local props = require("pinnacle.util").batch(requests)

    for i, prop in ipairs(props) do
        if prop.focused then
            return handles[i]
        end
    end

    return nil
end

---Begin moving this window using the specified mouse button.
---
---The button must be pressed at the time this method is called.
---If the button is lifted, the move will end.
---
---#### Example
---```lua
---Input.mousebind({ "super" }, "btn_left", function()
---    Window.begin_move("btn_left")
---end)
---```
---@param button MouseButton The button that will initiate the move
function window.begin_move(button)
    ---@diagnostic disable-next-line: redefined-local, invisible
    local button = require("pinnacle.input").mouse_button_values[button]
    local _, err = client:unary_request(window_service.MoveGrab, { button = button })

    if err then
        log:error(err)
    end
end

---Begin resizing this window using the specified mouse button.
---
---The button must be pressed at the time this method is called.
---If the button is lifted, the resize will end.
---
---#### Example
---```lua
---Input.mousebind({ "super" }, "btn_right", function()
---    Window.begin_resize("btn_right")
---end)
---```
---@param button MouseButton The button that will initiate the resize
function window.begin_resize(button)
    ---@diagnostic disable-next-line: redefined-local, invisible
    local button = require("pinnacle.input").mouse_button_values[button]
    local _, err = client:unary_request(window_service.ResizeGrab, { button = button })

    if err then
        log:error(err)
    end
end

---@class WindowRuleCondition
---@field any WindowRuleCondition[]?
---@field all WindowRuleCondition[]?
---@field classes string[]?
---@field titles string[]?
---@field tags TagHandle[]?

---@enum (key) WindowState
local window_state = {
    tiled = window_v0alpha1.WindowState.WINDOW_STATE_TILED,
    floating = window_v0alpha1.WindowState.WINDOW_STATE_FLOATING,
    fullscreen = window_v0alpha1.WindowState.WINDOW_STATE_FULLSCREEN,
    maximized = window_v0alpha1.WindowState.WINDOW_STATE_MAXIMIZED,
}
require("pinnacle.util").make_bijective(window_state)

---@class WindowRule
---@field output OutputHandle?
---@field tags TagHandle[]?
---@field floating boolean? -- Deprecated; use `state` with "floating" or "tiled" instead
---@field fullscreen_or_maximized FullscreenOrMaximized? -- Deprecated; use `state` with "fullscreen" or "maximized" instead
---@field x integer?
---@field y integer?
---@field width integer?
---@field height integer?
---@field decoration_mode ("client_side" | "server_side")?
---@field state WindowState?

---@enum (key) FullscreenOrMaximized
local fullscreen_or_maximized = {
    neither = window_v0alpha1.FullscreenOrMaximized.FULLSCREEN_OR_MAXIMIZED_NEITHER,
    fullscreen = window_v0alpha1.FullscreenOrMaximized.FULLSCREEN_OR_MAXIMIZED_FULLSCREEN,
    maximized = window_v0alpha1.FullscreenOrMaximized.FULLSCREEN_OR_MAXIMIZED_MAXIMIZED,
}
require("pinnacle.util").make_bijective(fullscreen_or_maximized)

---@param rule WindowRule
local function process_window_rule(rule)
    if rule.output then
        ---@diagnostic disable-next-line: assign-type-mismatch
        rule.output = rule.output.name
    end

    if rule.tags then
        local ids = {}
        for _, tg in ipairs(rule.tags) do
            table.insert(ids, tg.id)
        end
        rule.tags = ids
    end

    if rule.fullscreen_or_maximized then
        rule.fullscreen_or_maximized = fullscreen_or_maximized[rule.fullscreen_or_maximized]
    end

    if rule.decoration_mode then
        if rule.decoration_mode == "client_side" then
            ---@diagnostic disable-next-line: inject-field
            rule.ssd = false
        elseif rule.decoration_mode == "server_side" then
            ---@diagnostic disable-next-line: inject-field
            rule.ssd = true
        end
    end

    if rule.state then
        rule.state = window_state[rule.state]
    end
end

---@param cond WindowRuleCondition
local function process_window_rule_cond(cond)
    if cond.tags then
        local ids = {}
        for _, tg in ipairs(cond.tags) do
            table.insert(ids, tg.id)
        end
        cond.tags = ids
    end

    if cond.all then
        for _, con in ipairs(cond.all) do
            process_window_rule_cond(con)
        end
    end

    if cond.any then
        for _, con in ipairs(cond.any) do
            process_window_rule_cond(con)
        end
    end
end

---Add a window rule.
---
---A window rule defines what properties a window will spawn with given certain conditions.
---For example, if Firefox is spawned, you can set it to open on a specific tag.
---
---This method takes in a table with two keys:
---
--- - `cond`: The condition for `rule` to apply to a new window.
--- - `rule`: What gets applied to the new window if `cond` is true.
---
---There are some important mechanics you should know when using window rules:
---
--- - All children inside an `all` block must be true for the block to be true.
--- - At least one child inside an `any` block must be true for the block to be true.
--- - The outermost block of a window rule condition is implicitly an `all` block.
--- - Within an `all` block, all items in each array must be true for the attribute to be true.
--- - Within an `any` block, only one item in each array needs to be true for the attribute to be true.
---
---`cond` can be a bit confusing and quite table heavy. Examples are shown below for guidance.
---
---#### Examples
---```lua
--- -- A simple window rule. This one will cause Firefox to open on tag "Browser".
---Window.add_window_rule({
---    cond = { classes = { "firefox" } },
---    rule = { tags = { Tag.get("Browser") } },
---})
---
--- -- To apply rules when *all* provided conditions are true, use `all`.
--- -- `all` takes an array of conditions and checks if all are true.
--- -- The following will open Steam fullscreen only if it opens on tag "5".
---Window.add_window_rule({
---    cond = {
---        all = {
---            {
---                classes = { "steam" },
---                tags = { Tag.get("5") },
---            }
---        }
---    },
---    rule = { fullscreen_or_maximized = "fullscreen" },
---})
---
--- -- The outermost block of a `cond` is implicitly an `all` block.
--- -- Thus, the above can be shortened to:
---Window.add_window_rule({
---    cond = {
---        classes = { "steam" },
---        tags = { Tag.get("5") },
---    },
---    rule = { fullscreen_or_maximized = "fullscreen" },
---})
---
--- -- `any` also exists to allow at least one provided condition to match.
--- -- The following will open either xterm or Alacritty floating.
---Window.add_window_rule({
---    cond = {
---        any = { { classes = { "xterm", "Alacritty" } } }
---    },
---    rule = { floating = true },
---})
---
--- -- You can arbitrarily nest `any` and `all` to achieve desired logic.
--- -- The following will open Discord, Thunderbird, or Firefox floating if they
--- -- open on either *all* of tags "A", "B", and "C" or both tags "1" and "2".
---Window.add_window_rule({
---    cond = {
---        all = { -- This `all` block is needed because the outermost block cannot be an array.
---            { any = {
---                { classes = { "firefox", "thunderbird", "discord" } }
---            } },
---            { any = {
---                -- Because `tag` is inside an `all` block,
---                -- the window must have all these tags for this to be true.
---                -- If it was in an `any` block, only one tag would need to match.
---                { all = {
---                    { tags = { Tag.get("A"), Tag.get("B"), Tag.get("C") } }
---                } },
---                { all = {
---                    { tags = { Tag.get("1"), Tag.get("2") } }
---                } },
---            } }
---        }
---    },
---    rule = { floating = true },
---})
---```
---
---@param rule { cond: WindowRuleCondition, rule: WindowRule } The condition and rule
function window.add_window_rule(rule)
    process_window_rule(rule.rule)

    process_window_rule_cond(rule.cond)

    local _, err = client:unary_request(window_service.AddWindowRule, {
        cond = rule.cond,
        rule = rule.rule,
    })

    if err then
        log:error(err)
    end
end

local signal_name_to_SignalName = {
    pointer_enter = "WindowPointerEnter",
    pointer_leave = "WindowPointerLeave",
}

---@class WindowSignal Signals related to compositor events.
---@field pointer_enter fun(window: WindowHandle)? The pointer entered a window.
---@field pointer_leave fun(window: WindowHandle)? The pointer left a window.

---Connect to a window signal.
---
---The compositor sends signals about various events. Use this function to run a callback when
---some window signal occurs.
---
---This function returns a table of signal handles with each handle stored at the same key used
---to connect to the signal. See `SignalHandles` for more information.
---
---# Example
---```lua
---Window.connect_signal({
---    pointer_enter = function(window)
---        print("Pointer entered", window:class())
---    end
---})
---```
---
---@param signals WindowSignal The signal you want to connect to
---
---@return SignalHandles signal_handles Handles to every signal you connected to wrapped in a table, with keys being the same as the connected signal.
---
---@see SignalHandles.disconnect_all - To disconnect from these signals
function window.connect_signal(signals)
    ---@diagnostic disable-next-line: invisible
    local handles = require("pinnacle.signal").handles.new({})

    for signal, callback in pairs(signals) do
        require("pinnacle.signal").add_callback(signal_name_to_SignalName[signal], callback)
        local handle =
            ---@diagnostic disable-next-line: invisible
            require("pinnacle.signal").handle.new(signal_name_to_SignalName[signal], callback)
        handles[signal] = handle
    end

    return handles
end

------------------------------------------------------------------------

---Send a close request to this window.
---
---#### Example
---```lua
---local focused = Window.get_focused()
---if focused then focused:close() end
---```
function WindowHandle:close()
    local _, err = client:unary_request(window_service.Close, { window_id = self.id })

    if err then
        log:error(err)
    end
end

---Set this window's location and/or size.
---
---The coordinate system has the following axes:
---```
---       ^ -y
---       |
--- -x <--+--> +x
---       |
---       v +y
---```
---
---*Tiled windows will not reflect these changes.*
---This method only applies to this window's floating geometry.
---
---#### Example
---```lua
---local focused = Window.get_focused()
---if focused then
---    focused:set_floating(true)                     -- `set_geometry` only applies to floating geometry.
---
---    focused:set_geometry({ x = 50, y = 300 })      -- Move this window to (50, 300)
---    focused:set_geometry({ y = 0, height = 1080 }) -- Move this window to y = 0 and make its height 1080 pixels
---    focused:set_geometry({})                       -- Do nothing useful
---end
---```
---@param geo { x: integer?, y: integer?, width: integer?, height: integer? } The new location and/or size
function WindowHandle:set_geometry(geo)
    local _, err =
        client:unary_request(window_service.SetGeometry, { window_id = self.id, geometry = geo })

    if err then
        log:error(err)
    end
end

---Set this window to fullscreen or not.
---
---#### Example
---```lua
---local focused = Window.get_focused()
---if focused then
---    focused:set_fullscreen(true)
---    focused:set_fullscreen(false)
---end
---```
---
---@param fullscreen boolean
function WindowHandle:set_fullscreen(fullscreen)
    local _, err = client:unary_request(
        window_service.SetFullscreen,
        { window_id = self.id, set_or_toggle = set_or_toggle[fullscreen] }
    )

    if err then
        log:error(err)
    end
end

---Toggle this window to and from fullscreen.
---
---#### Example
---```lua
---local focused = Window.get_focused()
---if focused then
---    focused:toggle_fullscreen()
---end
---```
function WindowHandle:toggle_fullscreen()
    local _, err = client:unary_request(
        window_service.SetFullscreen,
        { window_id = self.id, set_or_toggle = set_or_toggle.TOGGLE }
    )

    if err then
        log:error(err)
    end
end

---Set this window to maximized or not.
---
---#### Example
---```lua
---local focused = Window.get_focused()
---if focused then
---    focused:set_maximized(true)
---    focused:set_maximized(false)
---end
---```
---
---@param maximized boolean
function WindowHandle:set_maximized(maximized)
    local _, err = client:unary_request(
        window_service.SetMaximized,
        { window_id = self.id, set_or_toggle = set_or_toggle[maximized] }
    )

    if err then
        log:error(err)
    end
end

---Toggle this window to and from maximized.
---
---#### Example
---```lua
---local focused = Window.get_focused()
---if focused then
---    focused:toggle_maximized()
---end
---```
function WindowHandle:toggle_maximized()
    local _, err = client:unary_request(
        window_service.SetMaximized,
        { window_id = self.id, set_or_toggle = set_or_toggle.TOGGLE }
    )

    if err then
        log:error(err)
    end
end

---Set this window to floating or not.
---
---#### Example
---```lua
---local focused = Window.get_focused()
---if focused then
---    focused:set_floating(true)
---    focused:set_floating(false)
---end
---```
---
---@param floating boolean
function WindowHandle:set_floating(floating)
    local _, err = client:unary_request(
        window_service.SetFloating,
        { window_id = self.id, set_or_toggle = set_or_toggle[floating] }
    )

    if err then
        log:error(err)
    end
end

---Toggle this window to and from floating.
---
---#### Example
---```lua
---local focused = Window.get_focused()
---if focused then
---    focused:toggle_floating()
---end
---```
function WindowHandle:toggle_floating()
    local _, err = client:unary_request(
        window_service.SetFloating,
        { window_id = self.id, set_or_toggle = set_or_toggle.TOGGLE }
    )

    if err then
        log:error(err)
    end
end

---Focus or unfocus this window.
---
---#### Example
---```lua
---local focused = Window.get_focused()
---if focused then
---    focused:set_focused(false)
---end
---```
---
---@param focused boolean
function WindowHandle:set_focused(focused)
    local _, err = client:unary_request(
        window_service.SetFocused,
        { window_id = self.id, set_or_toggle = set_or_toggle[focused] }
    )

    if err then
        log:error(err)
    end
end

---Toggle this window to and from focused.
---
---#### Example
---```lua
---local focused = Window.get_focused()
---if focused then
---    focused:toggle_focused()
---end
---```
function WindowHandle:toggle_focused()
    local _, err = client:unary_request(
        window_service.SetFocused,
        { window_id = self.id, set_or_toggle = set_or_toggle.TOGGLE }
    )

    if err then
        log:error(err)
    end
end

---Move this window to the specified tag.
---
---This will remove all tags from this window and tag it with `tag`.
---
---#### Example
---```lua
--- -- Assume the focused output has the tag "Tag"
---local focused = Window.get_focused()
---if focused then
---    focused:move_to_tag(Tag.get("Tag"))
---end
---```
---
---@param tag TagHandle The tag to move this window to
function WindowHandle:move_to_tag(tag)
    local _, err =
        client:unary_request(window_service.MoveToTag, { window_id = self.id, tag_id = tag.id })

    if err then
        log:error(err)
    end
end

---Tag or untag the given tag on this window.
---
---#### Example
---```lua
--- -- Assume the focused output has the tag "Tag"
---local focused = Window.get_focused()
---if focused then
---    local tag = Tag.get("Tag")
---
---    focused:set_tag(tag, true)
---    -- `focused` now has tag "Tag"
---    focused:set_tag(tag, false)
---    -- `focused` no longer has tag "Tag"
---end
---```
---
---@param tag TagHandle The tag to set or unset
---@param set boolean
function WindowHandle:set_tag(tag, set)
    local _, err = client:unary_request(
        window_service.SetTag,
        { window_id = self.id, tag_id = tag.id, set_or_toggle = set_or_toggle[set] }
    )

    if err then
        log:error(err)
    end
end

---Toggle the given tag on this window.
---
---#### Example
---```lua
--- -- Assume the focused output has the tag "Tag"
---local focused = Window.get_focused()
---if focused then
---    local tag = Tag.get("Tag")
---    focused:set_tag(tag, false)
---
---    focused:toggle_tag(tag)
---    -- `focused` now has tag "Tag"
---    focused:toggle_tag(tag)
---    -- `focused` no longer has tag "Tag"
---end
---```
---
---@param tag TagHandle The tag to toggle
function WindowHandle:toggle_tag(tag)
    local _, err = client:unary_request(
        window_service.SetTag,
        { window_id = self.id, tag_id = tag.id, set_or_toggle = set_or_toggle.TOGGLE }
    )

    if err then
        log:error(err)
    end
end

---Raise a window.
---
---This will raise a window all the way to the top of the z-stack.
---
---#### Example
---```lua
---local focused = Window.get_focused()
---if focused then
---    focused:raise()
---end
---```
function WindowHandle:raise()
    local _, err = client:unary_request(window_service.Raise, { window_id = self.id })

    if err then
        log:error(err)
    end
end

---Returns whether or not this window is on an active tag.
---
---@return boolean
function WindowHandle:is_on_active_tag()
    local tags = self:tags() or {}

    ---@type (fun(): boolean)[]
    local batch = {}

    for i, tg in ipairs(tags) do
        batch[i] = function()
            return tg:active() or false
        end
    end

    local actives = require("pinnacle.util").batch(batch)

    for _, active in ipairs(actives) do
        if active then
            return true
        end
    end

    return false
end

---@class WindowProperties
---@field geometry { x: integer?, y: integer?, width: integer?, height: integer? }? The location and size of the window
---@field class string? The window's class
---@field title string? The window's title
---@field focused boolean? Whether or not the window is focused
---@field floating boolean? Whether or not the window is floating
---@field fullscreen_or_maximized FullscreenOrMaximized? Whether the window is fullscreen, maximized, or neither
---@field tags TagHandle[] The tags the window has
---@field state WindowState?

---Get all the properties of this window.
---
---@return WindowProperties
function WindowHandle:props()
    local response, err =
        client:unary_request(window_service.GetProperties, { window_id = self.id })

    if err then
        log:error(err)
        return {} -- TODO: make nullable
    end

    ---@cast response pinnacle.window.v0alpha1.GetPropertiesResponse

    ---@type WindowProperties
    local props = {
        geometry = response.geometry,
        class = response.class,
        title = response.title,
        focused = response.focused,
        floating = response.floating,
        fullscreen_or_maximized = fullscreen_or_maximized[response.fullscreen_or_maximized],
        tags = response.tag_ids
                ---@diagnostic disable-next-line: invisible
                and require("pinnacle.tag").handle.new_from_table(response.tag_ids)
            or {},
        state = response.state and window_state[response.state],
    }

    return props
end

---Get this window's location and size.
---
---Shorthand for `handle:props().geometry`.
---
---@return { x: integer?, y: integer?, width: integer?, height: integer? }?
function WindowHandle:geometry()
    return self:props().geometry
end

---Get this window's class.
---
---Shorthand for `handle:props().class`.
---
---@return string?
function WindowHandle:class()
    return self:props().class
end

---Get this window's title.
---
---Shorthand for `handle:props().title`.
---
---@return string?
function WindowHandle:title()
    return self:props().title
end

---Get whether or not this window is focused.
---
---Shorthand for `handle:props().focused`.
---
---@return boolean?
function WindowHandle:focused()
    return self:props().focused
end

---Get whether or not this window is floating.
---
---Shorthand for `handle:props().floating`.
---
---@return boolean
function WindowHandle:floating()
    return self:props().state == "floating"
end

---Get whether this window is tiled.
---
---@return boolean
function WindowHandle:tiled()
    return self:props().state == "tiled"
end

---Get whether this window is fullscreen.
---
---@return boolean
function WindowHandle:fullscreen()
    return self:props().state == "fullscreen"
end

---Get whether this window is maximized.
---
---@return boolean
function WindowHandle:maximized()
    return self:props().state == "maximized"
end

---Deprecated; use the `fullscreen` or `maximized` methods instead.
---
---Get whether this window is fullscreen, maximized, or neither.
---
---Shorthand for `handle:props().fullscreen_or_maximized`.
---
---@return FullscreenOrMaximized?
---@deprecated
function WindowHandle:fullscreen_or_maximized()
    return self:props().fullscreen_or_maximized
end

---Get all tags on this window.
---
---Shorthand for `handle:props().tags`.
---
---@return TagHandle[]?
function WindowHandle:tags()
    return self:props().tags
end

---Create a new `WindowHandle` from an id.
---@param window_id integer
---@return WindowHandle
function window_handle.new(window_id)
    ---@type WindowHandle
    local self = {
        id = window_id,
    }
    setmetatable(self, { __index = WindowHandle })
    return self
end

---@param window_ids integer[]
---
---@return WindowHandle[]
function window_handle.new_from_table(window_ids)
    ---@type WindowHandle[]
    local handles = {}

    for _, id in ipairs(window_ids) do
        table.insert(handles, window_handle.new(id))
    end

    return handles
end

return window
