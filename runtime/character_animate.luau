-- Headless subset of the default character animation controller.
-- It drives AnimationTrack state, but does not simulate movement or joints.
local character = script.Parent
local humanoid = character:WaitForChild("Humanoid")
local animator = humanoid:FindFirstChildOfClass("Animator")
if not animator then return end

local track
local current
local function play(name)
    if current == name then return end
    local set = script:FindFirstChild(name)
    local animation = set and set:FindFirstChildOfClass("Animation")
    if not animation then return end
    if track then track:Stop(0.1) end
    track = animator:LoadAnimation(animation)
    track.Looped = name ~= "jump"
    track:Play(0.1)
    current = name
end

local stateConnection = humanoid.StateChanged:Connect(function(_, state)
    if state == Enum.HumanoidStateType.Dead then
        if track then track:Stop(0.1) end
        current = nil
    elseif state == Enum.HumanoidStateType.Jumping then play("jump")
    elseif state == Enum.HumanoidStateType.Freefall then play("fall")
    elseif state == Enum.HumanoidStateType.Climbing then play("climb")
    elseif state == Enum.HumanoidStateType.Swimming then play("swim")
    elseif state == Enum.HumanoidStateType.Seated then play("sit")
    elseif state == Enum.HumanoidStateType.Running or state == Enum.HumanoidStateType.RunningNoPhysics then play("walk")
    else play("idle") end
end)

play("idle")
while character.Parent and script.Parent do task.wait(1) end
stateConnection:Disconnect()
if track then track:Stop(0.1) end
