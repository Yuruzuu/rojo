return function()
	-- Services
	local PatchSet = require(script.Parent.Parent.PatchSet)
	local InstanceMap = require(script.Parent.Parent.InstanceMap)

	local createPatchSet = require(script.Parent.createPatchSet)

	it("should return a patch", function()
		local patch = createPatchSet(InstanceMap.new(), {})

		assert(PatchSet.validate(patch))
	end)

	it("should contain updates for every instance with property changes", function()
		local instanceMap = InstanceMap.new()

		local part1 = Instance.new("Part")
		instanceMap:insert("PART_1", part1)

		local part2 = Instance.new("Part")
		instanceMap:insert("PART_2", part2)

		local changes = {
			[part1] = {
				Position = true,
				Size = true,
				Color = true,
			},
			[part2] = {
				CFrame = true,
				Velocity = true,
				Transparency = true,
			},
		}

		local patch = createPatchSet(instanceMap, changes)

		expect(#patch.updated).to.equal(2)
	end)

	it("should not contain any updates for removed instances", function()
		local instanceMap = InstanceMap.new()

		local part1 = Instance.new("Part")
		instanceMap:insert("PART_1", part1)

		local changes = {
			[part1] = {
				Parent = true,
				Position = true,
				Size = true,
			},
		}

		local patch = createPatchSet(instanceMap, changes)

		expect(#patch.removed).to.equal(1)
		expect(#patch.updated).to.equal(0)
	end)

	it("should remove instances from the property change table", function()
		local instanceMap = InstanceMap.new()

		local part1 = Instance.new("Part")
		instanceMap:insert("PART_1", part1)

		local changes = {
			[part1] = {},
		}

		createPatchSet(instanceMap, changes)

		expect(next(changes)).to.equal(nil)
	end)

	it("should consume failed reference edits, preserve unrelated renames, and allow later batches", function()
		local Map = InstanceMap.new()

		local Reference = Instance.new("ObjectValue")
		local Target = Instance.new("Folder")

		Map:insert("REFERENCE", Reference)
		Map:insert("TARGET", Target)

		Reference.Value = Target
		Target.Name = "RenamedTarget"

		local Changes = {
			[Reference] = { Value = true },
			[Target] = { Name = true },
		}

		local Patch = createPatchSet(Map, Changes)

		expect(#Patch.updated).to.equal(1)
		expect(Patch.updated[1].id).to.equal("TARGET")
		expect(Patch.updated[1].changedName).to.equal("RenamedTarget")
		expect(next(Changes)).to.equal(nil)

		Reference.Name = "RenamedReference"
		Changes[Reference] = { Name = true }

		local LaterPatch = createPatchSet(Map, Changes)

		expect(#LaterPatch.updated).to.equal(1)
		expect(LaterPatch.updated[1].id).to.equal("REFERENCE")
		expect(LaterPatch.updated[1].changedName).to.equal("RenamedReference")
		expect(next(Changes)).to.equal(nil)

		Map:stop()
		Reference:Destroy()
		Target:Destroy()
	end)

	it("should consume malformed property names and preserve valid changes", function()
		local Map = InstanceMap.new()

		local Part = Instance.new("Part")

		Map:insert("PART", Part)
		Part.Name = "RenamedPart"

		local Changes = {
			[Part] = { NonExistentProperty = true, Name = true },
		}

		local Patch = createPatchSet(Map, Changes)

		expect(#Patch.updated).to.equal(1)
		expect(Patch.updated[1].id).to.equal("PART")
		expect(Patch.updated[1].changedName).to.equal("RenamedPart")
		expect(next(Patch.updated[1].changedProperties)).to.equal(nil)
		expect(next(Changes)).to.equal(nil)

		Map:stop()
		Part:Destroy()
	end)
end
