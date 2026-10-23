-- Services
local ScriptEditorService = game:GetService("ScriptEditorService")

-- Utilities
local Packages = script.Parent.Parent.Parent.Packages
local RbxDom = require(Packages.RbxDom)

local ScriptSourceInitializer = {}

function ScriptSourceInitializer.Initialize(SourceContainer, Source)
	local Descriptor = RbxDom.findCanonicalPropertyDescriptor(SourceContainer.ClassName, "Source")
	if Descriptor == nil or Descriptor.scriptability == "None" or Descriptor.scriptability == "Read" then
		return false
	end

	-- The vendored descriptor starts a task and cannot report completion or failure.
	local Success = pcall(function()
		ScriptEditorService:UpdateSourceAsync(SourceContainer, function()
			return Source
		end)
	end)

	return Success
end

return ScriptSourceInitializer
