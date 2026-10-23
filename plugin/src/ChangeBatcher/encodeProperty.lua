-- Services
local Packages = script.Parent.Parent.Parent.Packages
local Log = require(Packages.Log)
local RbxDom = require(Packages.RbxDom)

return function(instance, propertyName, propertyDescriptor)
	local readSuccess, readResult = propertyDescriptor:read(instance)

	if not readSuccess then
		Log.warn("Could not sync back property {:?}.{}: {}", instance, propertyName, readResult)
		return false, nil
	end

	local dataType = propertyDescriptor.dataType
	local CallSuccess, encodeSuccess, encodeResult = pcall(RbxDom.EncodedValue.encode, readResult, dataType)

	if not CallSuccess or not encodeSuccess then
		Log.warn(
			"Could not sync back property {:?}.{}: {}",
			instance,
			propertyName,
			if CallSuccess then encodeResult else encodeSuccess
		)
		return false, nil
	end

	return true, encodeResult
end
