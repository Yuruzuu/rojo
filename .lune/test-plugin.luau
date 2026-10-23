-- Services

-- Utilities
local FileSystem = require("@lune/fs")
local Luau = require("@lune/luau")
local Process = require("@lune/process")
local Roblox = require("@lune/roblox")
local Serde = require("@lune/serde")
local Task = require("@lune/task")

local ModelPath = Process.args[1]
local TemporaryModel = ModelPath == nil
if TemporaryModel then
	local TemporaryDirectory = Process.env.TEMP or Process.env.TMPDIR or "/tmp"
	ModelPath = `{TemporaryDirectory}/rojo-plugin-tests-{os.time()}-{math.random(1, 2 ^ 30)}.rbxm`
	assert(not FileSystem.isFile(ModelPath), "Temporary plugin model already exists")

	local Build = Process.exec("rojo", { "build", "plugin.project.json", "--output", ModelPath })
	assert(Build.ok, `Could not build plugin for testing: {Build.stderr}`)
end

local Root = Roblox.deserializeModel(FileSystem.readFile(ModelPath))[1]
if TemporaryModel then
	FileSystem.removeFile(ModelPath)
end

-- Service adapters exercise module behavior offline, without Studio's event loop or security model.
local DataModel = Roblox.Instance.new("DataModel")
Root.Parent = DataModel:GetService("ReplicatedStorage")

local MockTypes = setmetatable({}, { __mode = "k" })
local Signals = setmetatable({}, { __mode = "k" })
local NativeInstances = {}
local InstanceProxies = {}

local function WrapInstance(Native)
	if Native == nil then
		return Native
	end
	if InstanceProxies[Native] then
		return InstanceProxies[Native]
	end

	local Cleared = false
	local Proxy = setmetatable({}, {
		__index = function(_, Key)
			if Key == "Value" and Native.ClassName == "ObjectValue" and Cleared then
				return nil
			end

			local Value = Native[Key]
			if type(Value) == "function" then
				return function(_, ...)
					local Arguments = table.pack(...)
					for Index = 1, Arguments.n do
						Arguments[Index] = NativeInstances[Arguments[Index]] or Arguments[Index]
					end
					local Results = table.pack(Value(Native, table.unpack(Arguments, 1, Arguments.n)))
					for Index = 1, Results.n do
						if typeof(Results[Index]) == "Instance" then
							Results[Index] = WrapInstance(Results[Index])
						elseif Key == "GetChildren" or Key == "GetDescendants" then
							for ChildIndex, Child in Results[Index] do
								Results[Index][ChildIndex] = WrapInstance(Child)
							end
						end
					end

					return table.unpack(Results, 1, Results.n)
				end
			end

			return if typeof(Value) == "Instance" then WrapInstance(Value) else Value
		end,
		__newindex = function(_, Key, Value)
			-- Lune rejects nil Ref assignments; model Studio's ObjectValue clearing explicitly.
			if Key == "Value" and Native.ClassName == "ObjectValue" then
				Cleared = Value == nil
				if Cleared then
					return
				end
			end
			Native[Key] = NativeInstances[Value] or Value
		end,
	})
	NativeInstances[Proxy] = Native
	InstanceProxies[Native] = Proxy
	MockTypes[Proxy] = "Instance"

	return Proxy
end

local function NewSignal()
	local Callbacks = {}
	local Signal = {}
	MockTypes[Signal] = "RBXScriptSignal"

	function Signal:Connect(Callback)
		local Connection = { Connected = true }
		MockTypes[Connection] = "RBXScriptConnection"
		Callbacks[Connection] = Callback

		function Connection.Disconnect()
			Connection.Connected = false
			Callbacks[Connection] = nil
		end

		return Connection
	end

	function Signal:Fire(...)
		for Connection, Callback in table.clone(Callbacks) do
			if Connection.Connected then
				Task.spawn(Callback, ...)
			end
		end
	end

	function Signal:Wait()
		local Thread = coroutine.running()
		local Connection
		Connection = self:Connect(function(...)
			Connection:Disconnect()
			Task.spawn(Thread, ...)
		end)

		return coroutine.yield()
	end

	return Signal
end

local function GetSignal(Owner, Name)
	local OwnerSignals = Signals[Owner]
	if not OwnerSignals then
		OwnerSignals = {}
		Signals[Owner] = OwnerSignals
	end
	OwnerSignals[Name] = OwnerSignals[Name] or NewSignal()

	return OwnerSignals[Name]
end

Roblox.implementProperty("Instance", "Changed", function(Owner)
	return GetSignal(Owner, "Changed")
end)
Roblox.implementMethod("Instance", "GetPropertyChangedSignal", GetSignal)
for _, Name in { "Heartbeat", "RenderStepped", "Stepped" } do
	Roblox.implementProperty("RunService", Name, function(Owner)
		return GetSignal(Owner, Name)
	end)
end
for Name, Value in { IsRunning = false, IsClient = false, IsServer = true, IsStudio = true, IsEdit = true } do
	Roblox.implementMethod("RunService", Name, function()
		return Value
	end)
end
Roblox.implementMethod("HttpService", "JSONDecode", function(_, Value)
	return Serde.decode("json", Value)
end)
Roblox.implementMethod("HttpService", "JSONEncode", function(_, Value)
	return Serde.encode("json", Value)
end)
Roblox.implementMethod("HttpService", "RequestAsync", function()
	error("Network requests are unavailable in offline plugin tests")
end)
Roblox.implementMethod("ScriptEditorService", "GetEditorSource", function(_, SourceContainer)
	return SourceContainer.Source
end)
Roblox.implementMethod("ScriptEditorService", "UpdateSourceAsync", function(_, SourceContainer, Callback)
	SourceContainer.Source = Callback(SourceContainer.Source)
end)
Roblox.implementMethod("TestService", "Error", function(_, Message)
	warn(Message)
end)
local Selection = {}
Roblox.implementMethod("Selection", "Get", function()
	return table.clone(Selection)
end)
Roblox.implementMethod("Selection", "Set", function(_, Instances)
	Selection = table.clone(Instances)
end)

local Cache = {}
local Loading = {}
local SharedGlobals = {}
local BaseEnvironment = {
	game = DataModel,
	workspace = DataModel:GetService("Workspace"),
	task = Task,
	tick = os.clock,
	wait = Task.wait,
	spawn = Task.spawn,
	_G = SharedGlobals,
	typeof = function(Value)
		return MockTypes[Value] or typeof(Value)
	end,
}
for _, Name in
	{
		"Instance",
		"Enum",
		"Axes",
		"BrickColor",
		"CFrame",
		"Color3",
		"ColorSequence",
		"ColorSequenceKeypoint",
		"Content",
		"Faces",
		"Font",
		"NumberRange",
		"NumberSequence",
		"NumberSequenceKeypoint",
		"PhysicalProperties",
		"Ray",
		"Rect",
		"Region3",
		"Region3int16",
		"UDim",
		"UDim2",
		"Vector2",
		"Vector2int16",
		"Vector3",
		"Vector3int16",
	}
do
	BaseEnvironment[Name] = Roblox[Name]
end
BaseEnvironment.Instance = {
	new = function(ClassName, Parent)
		local Instance = Roblox.Instance.new(ClassName)
		-- Lune names these after their class; Studio creates ValueBase instances as "Value".
		if Instance:IsA("ValueBase") then
			Instance.Name = "Value"
		end
		Instance.Parent = NativeInstances[Parent] or Parent

		return WrapInstance(Instance)
	end,
}

local function RequireModule(Module)
	if typeof(Module) ~= "Instance" then
		return require(Module)
	end
	local Path = Module:GetFullName()
	if Cache[Path] ~= nil then
		return Cache[Path]
	end
	assert(not Loading[Path], `Circular require: {Path}`)
	Loading[Path] = true

	local Environment = table.clone(BaseEnvironment)
	Environment.script = Module
	Environment.require = RequireModule
	local Chunk = Luau.load(Module.Source, { debugName = Path, environment = Environment })
	local Result = Chunk()
	Cache[Path] = Result
	Loading[Path] = nil

	return Result
end

local RbxDom = RequireModule(Root.Packages.RbxDom)
local EncodeValue = RbxDom.EncodedValue.encode
RbxDom.EncodedValue.encode = function(Value, DataType)
	-- Lune lacks the deprecated lowercase Color3 fields used by the vendored encoder.
	if DataType == "Color3" and typeof(Value) == "Color3" then
		return true, { Color3 = { Value.R, Value.G, Value.B } }
	end

	return EncodeValue(Value, DataType)
end

local TestEZ = RequireModule(Root.Packages.TestEZ)
local Specs = {}
local Paths = if #Process.args > 1
	then { table.unpack(Process.args, 2) }
	else {
		"Plugin/ApiContext.spec",
		"Plugin/ServeSession.spec",
		"Plugin/Version.spec",
		"Plugin/orderSwaps.spec",
		"Plugin/ChangeBatcher/encodePatchUpdate.spec",
		"Plugin/ChangeBatcher/createPatchSet.spec",
		"Plugin/Reconciler/applyPatch.spec",
		"Plugin/Reconciler/diff.spec",
		"Plugin/Reconciler/reify.spec",
	}
for _, Path in Paths do
	local Module = Root
	for Part in Path:gmatch("[^/]+") do
		Module = assert(Module:FindFirstChild(Part), `Missing test path: {Path}`)
	end
	table.insert(Specs, Module)
end

local Results = TestEZ.TestBootstrap:run(Specs)
assert(Results.failureCount == 0 and #Results.errors == 0, "Offline plugin tests failed")
print(`OFFLINE RESULT: {Results.successCount} passed, {Results.failureCount} failed, {Results.skippedCount} skipped`)
