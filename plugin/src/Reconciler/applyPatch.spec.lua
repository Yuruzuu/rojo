return function()
	-- Services
	local applyPatch = require(script.Parent.applyPatch)
	local GetProperty = require(script.Parent.getProperty)
	local ScriptSourceInitializer = require(script.Parent.ScriptSourceInitializer)

	local InstanceMap = require(script.Parent.Parent.InstanceMap)
	local PatchSet = require(script.Parent.Parent.PatchSet)
	local RbxDom = require(script.Parent.Parent.Parent.Packages.RbxDom)

	local function WithSourceScriptability(ClassName, Scriptability, Callback)
		local FindDescriptor = RbxDom.findCanonicalPropertyDescriptor

		RbxDom.findCanonicalPropertyDescriptor = function(RequestedClassName, PropertyName)
			local Descriptor = FindDescriptor(RequestedClassName, PropertyName)
			if RequestedClassName == ClassName and PropertyName == "Source" and Descriptor ~= nil then
				Descriptor.scriptability = Scriptability
			end

			return Descriptor
		end

		local Success, Result = pcall(Callback)
		RbxDom.findCanonicalPropertyDescriptor = FindDescriptor

		assert(Success, tostring(Result))

		return Result
	end

	local container = Instance.new("Folder")

	local tempContainer = Instance.new("Folder")
	local function wasRemoved(instance)
		-- If an instance was destroyed, its parent property is locked.
		-- If an instance was removed, its parent property is nil.
		-- We need to ensure we only remove, so that ChangeHistoryService can still Undo.

		local isParentUnlocked = pcall(function()
			local oldParent = instance.Parent
			instance.Parent = tempContainer
			instance.Parent = oldParent
		end)

		return instance.Parent == nil and isParentUnlocked
	end

	beforeEach(function()
		container:ClearAllChildren()
	end)

	afterAll(function()
		container:Destroy()
		tempContainer:Destroy()
	end)

	local function NewScriptReplacement()
		local Source = "return 'original source'"
		local Root = Instance.new("ModuleScript")
		Root.Name = "Original"
		Root.Source = Source
		Root.Parent = container

		local Child = Instance.new("Folder")
		Child.Parent = Root

		local KnownInstances = InstanceMap.new()
		KnownInstances:insert("ROOT", Root)
		KnownInstances:insert("CHILD", Child)

		local Patch = PatchSet.newEmpty()
		table.insert(Patch.updated, {
			id = "ROOT",
			changedClassName = "Script",
			changedProperties = {
				Source = { String = "print('replacement source')" },
			},
		})

		return {
			Source = Source,
			Root = Root,
			Child = Child,
			KnownInstances = KnownInstances,
			Patch = Patch,
		}
	end

	local function WithPendingSourceInitialization(Fixture, Callback)
		local Initialize = ScriptSourceInitializer.Initialize
		ScriptSourceInitializer.Initialize = function(Replacement, Source)
			local Outcome = coroutine.yield(Replacement, Source)
			if Outcome == "error" then
				error("Source initialization failed after yielding")
			end

			return Outcome and Initialize(Replacement, Source)
		end

		local PatchThread = coroutine.create(applyPatch)
		local Success, Result = pcall(function()
			local Started, Replacement, Source = coroutine.resume(PatchThread, Fixture.KnownInstances, Fixture.Patch)
			assert(Started, tostring(Replacement))
			expect(coroutine.status(PatchThread)).to.equal("suspended")
			expect(Source).to.equal(Fixture.Patch.updated[1].changedProperties.Source.String)
			expect(Replacement.Parent).to.equal(nil)
			expect(#Replacement:GetChildren()).to.equal(0)
			expect(Fixture.Root.Parent).to.equal(container)
			expect(Fixture.Root.Source).to.equal(Fixture.Source)
			expect(Fixture.Child.Parent).to.equal(Fixture.Root)
			expect(Fixture.KnownInstances.fromIds.ROOT).to.equal(Fixture.Root)
			expect(Fixture.KnownInstances.fromInstances[Fixture.Root]).to.equal("ROOT")
			expect(Fixture.KnownInstances.fromIds.CHILD).to.equal(Fixture.Child)
			expect(Fixture.KnownInstances.fromInstances[Replacement]).to.equal(nil)
			expect(#container:GetChildren()).to.equal(1)

			Callback(PatchThread, Replacement)
		end)
		ScriptSourceInitializer.Initialize = Initialize

		if coroutine.status(PatchThread) ~= "dead" then
			coroutine.close(PatchThread)
		end

		assert(Success, tostring(Result))
	end

	it("should return an empty patch if given an empty patch", function()
		local patch = applyPatch(InstanceMap.new(), PatchSet.newEmpty())
		assert(PatchSet.isEmpty(patch), "expected remaining patch to be empty")
	end)

	it("should remove instances listed for remove", function()
		local root = Instance.new("Folder")
		root.Name = "ROOT"
		root.Parent = container

		local child = Instance.new("Folder")
		child.Name = "Child"
		child.Parent = root

		local instanceMap = InstanceMap.new()
		instanceMap:insert("ROOT", root)
		instanceMap:insert("CHILD", child)

		local patch = PatchSet.newEmpty()
		table.insert(patch.removed, child)

		local unapplied = applyPatch(instanceMap, patch)
		assert(PatchSet.isEmpty(unapplied), "expected remaining patch to be empty")

		assert(not wasRemoved(root), "expected root to be left alone")
		assert(wasRemoved(child), "expected child to be removed")

		instanceMap:stop()
	end)

	it("should remove IDs listed for remove", function()
		local root = Instance.new("Folder")
		root.Name = "ROOT"
		root.Parent = container

		local child = Instance.new("Folder")
		child.Name = "Child"
		child.Parent = root

		local instanceMap = InstanceMap.new()
		instanceMap:insert("ROOT", root)
		instanceMap:insert("CHILD", child)

		local patch = PatchSet.newEmpty()
		table.insert(patch.removed, "CHILD")

		local unapplied = applyPatch(instanceMap, patch)
		assert(PatchSet.isEmpty(unapplied), "expected remaining patch to be empty")
		expect(instanceMap:size()).to.equal(1)

		assert(not wasRemoved(root), "expected root to be left alone")
		assert(wasRemoved(child), "expected child to be removed")

		instanceMap:stop()
	end)

	it("should add instances to the DOM", function()
		-- Many of the details of this functionality are instead covered by
		-- tests on reify, not here.

		local root = Instance.new("Folder")
		root.Name = "ROOT"
		root.Parent = container

		local instanceMap = InstanceMap.new()
		instanceMap:insert("ROOT", root)

		local patch = PatchSet.newEmpty()
		patch.added["CHILD"] = {
			Id = "CHILD",
			ClassName = "Model",
			Name = "Child",
			Parent = "ROOT",
			Children = { "GRANDCHILD" },
			Properties = {},
		}

		patch.added["GRANDCHILD"] = {
			Id = "GRANDCHILD",
			ClassName = "Part",
			Name = "Grandchild",
			Parent = "CHILD",
			Children = {},
			Properties = {},
		}

		local unapplied = applyPatch(instanceMap, patch)
		assert(PatchSet.isEmpty(unapplied), "expected remaining patch to be empty")
		expect(instanceMap:size()).to.equal(3)

		local child = root:FindFirstChild("Child")
		expect(child).to.be.ok()
		expect(child.ClassName).to.equal("Model")
		expect(child).to.equal(instanceMap.fromIds["CHILD"])

		local grandchild = child:FindFirstChild("Grandchild")
		expect(grandchild).to.be.ok()
		expect(grandchild.ClassName).to.equal("Part")
		expect(grandchild).to.equal(instanceMap.fromIds["GRANDCHILD"])
	end)

	it("should return unapplied additions when instances cannot be created", function()
		local root = Instance.new("Folder")
		root.Name = "ROOT"
		root.Parent = container

		local instanceMap = InstanceMap.new()
		instanceMap:insert("ROOT", root)

		local patch = PatchSet.newEmpty()
		patch.added["OOPSIE"] = {
			Id = "OOPSIE",
			-- Hopefully Roblox never makes an instance with this ClassName.
			ClassName = "UH OH",
			Name = "FUBAR",
			Parent = "ROOT",
			Children = {},
			Properties = {},
		}

		local unapplied = applyPatch(instanceMap, patch)
		expect(unapplied.added["OOPSIE"]).to.equal(patch.added["OOPSIE"])
		expect(instanceMap:size()).to.equal(1)
		expect(#root:GetChildren()).to.equal(0)
	end)

	it("should apply property changes to instances", function()
		local value = Instance.new("StringValue")
		value.Value = "HELLO"

		local instanceMap = InstanceMap.new()
		instanceMap:insert("VALUE", value)

		local patch = PatchSet.newEmpty()
		table.insert(patch.updated, {
			id = "VALUE",
			changedProperties = {
				Value = {
					String = "WORLD",
				},
			},
		})

		local unapplied = applyPatch(instanceMap, patch)
		assert(PatchSet.isEmpty(unapplied), "expected remaining patch to be empty")
		expect(value.Value).to.equal("WORLD")
	end)

	it("should clear an existing object reference when updated to a null ref", function()
		local Target = Instance.new("Folder")
		Target.Parent = container

		local Value = Instance.new("ObjectValue")
		Value.Value = Target
		Value.Parent = container

		local KnownInstances = InstanceMap.new()
		KnownInstances:insert("VALUE", Value)
		KnownInstances:insert("TARGET", Target)

		local Patch = PatchSet.newEmpty()
		table.insert(Patch.updated, {
			id = "VALUE",
			changedProperties = {
				Value = { Ref = "00000000000000000000000000000000" },
			},
		})

		local Unapplied = applyPatch(KnownInstances, Patch)
		assert(PatchSet.isEmpty(Unapplied), "expected null ref to be applied")
		expect(Value.Value).to.equal(nil)
		expect(KnownInstances.fromIds.TARGET).to.equal(Target)

		KnownInstances:stop()
	end)

	it("should retain an existing object reference when its updated ref is missing", function()
		local Target = Instance.new("Folder")
		Target.Parent = container

		local Value = Instance.new("ObjectValue")
		Value.Value = Target
		Value.Parent = container

		local KnownInstances = InstanceMap.new()
		KnownInstances:insert("VALUE", Value)

		local Patch = PatchSet.newEmpty()
		table.insert(Patch.updated, {
			id = "VALUE",
			changedProperties = {
				Value = { Ref = "MISSING" },
			},
		})

		local Unapplied = applyPatch(KnownInstances, Patch)
		expect(Value.Value).to.equal(Target)
		expect(#Unapplied.updated).to.equal(1)
		expect(Unapplied.updated[1].id).to.equal("VALUE")
		expect(Unapplied.updated[1].changedProperties.Value).to.equal(Patch.updated[1].changedProperties.Value)

		KnownInstances:stop()
	end)

	it("should recreate instances when changedClassName is set, preserving children", function()
		local root = Instance.new("Folder")
		root.Name = "Initial Root Name"
		root.Parent = container

		local child = Instance.new("Folder")
		child.Name = "Child"
		child.Parent = root

		local instanceMap = InstanceMap.new()
		instanceMap:insert("ROOT", root)
		instanceMap:insert("CHILD", child)

		local patch = PatchSet.newEmpty()
		table.insert(patch.updated, {
			id = "ROOT",
			changedName = "Updated Root Name",
			changedClassName = "StringValue",
			changedProperties = {
				Value = {
					String = "I am Root",
				},
			},
		})

		local unapplied = applyPatch(instanceMap, patch)
		assert(PatchSet.isEmpty(unapplied), "expected remaining patch to be empty")

		local newRoot = instanceMap.fromIds["ROOT"]
		assert(newRoot ~= root, "expected instance to be recreated")
		expect(newRoot.ClassName).to.equal("StringValue")
		expect(newRoot.Name).to.equal("Updated Root Name")
		expect(newRoot.Value).to.equal("I am Root")

		local newChild = newRoot:FindFirstChild("Child")
		assert(newChild ~= nil, "expected child to be present")
		assert(newChild == child, "expected child to be preserved")
	end)

	it("should preserve source and children when replacing a script without a source update", function()
		local Source = "return { Greeting = 'kept' }"
		local Root = Instance.new("ModuleScript")
		Root.Name = "Original"
		Root.Source = Source
		Root.Parent = container

		local Child = Instance.new("Folder")
		Child.Parent = Root

		local KnownInstances = InstanceMap.new()
		KnownInstances:insert("ROOT", Root)
		KnownInstances:insert("CHILD", Child)

		local Patch = PatchSet.newEmpty()
		table.insert(Patch.updated, {
			id = "ROOT",
			changedClassName = "Script",
			changedProperties = {},
		})

		local Unapplied = applyPatch(KnownInstances, Patch)
		assert(PatchSet.isEmpty(Unapplied), "expected script replacement to be applied")

		local Replacement = KnownInstances.fromIds.ROOT
		local ReadSourceSuccess, ReplacementSource = GetProperty(Replacement, "Source")
		assert(ReadSourceSuccess, "expected replacement source to be readable")
		expect(ReplacementSource).to.equal(Source)
		expect(Replacement.ClassName).to.equal("Script")
		expect(Replacement.Name).to.equal("Original")
		expect(Replacement.Parent).to.equal(container)
		expect(Child.Parent).to.equal(Replacement)
		expect(KnownInstances.fromIds.CHILD).to.equal(Child)
		expect(KnownInstances.fromInstances[Root]).to.equal(nil)
		expect(Root.Parent).to.equal(nil)
		expect(Patch.updated[1].changedProperties.Source).to.equal(nil)

		KnownInstances:stop()
		Root:Destroy()
	end)

	it("should honor explicit replacement source", function()
		local Root = Instance.new("ModuleScript")
		Root.Source = "return 'old source'"
		Root.Parent = container

		local KnownInstances = InstanceMap.new()
		KnownInstances:insert("ROOT", Root)

		local Patch = PatchSet.newEmpty()
		table.insert(Patch.updated, {
			id = "ROOT",
			changedClassName = "Script",
			changedProperties = {
				Source = { String = "print('new source')" },
			},
		})

		local Unapplied = WithSourceScriptability("ModuleScript", "None", function()
			return applyPatch(KnownInstances, Patch)
		end)
		assert(PatchSet.isEmpty(Unapplied), "expected explicit replacement source to be applied")

		local Replacement = KnownInstances.fromIds.ROOT
		local ReadSourceSuccess, ReplacementSource = GetProperty(Replacement, "Source")
		assert(ReadSourceSuccess, "expected replacement source to be readable")
		expect(ReplacementSource).to.equal("print('new source')")

		KnownInstances:stop()
		Root:Destroy()
	end)

	it("should publish a replacement only after delayed source initialization succeeds", function()
		local Fixture = NewScriptReplacement()

		WithPendingSourceInitialization(Fixture, function(PatchThread, Replacement)
			local Completed, Unapplied = coroutine.resume(PatchThread, true)
			assert(Completed, tostring(Unapplied))
			expect(coroutine.status(PatchThread)).to.equal("dead")
			assert(PatchSet.isEmpty(Unapplied), "expected initialized replacement to be applied")
			expect(Fixture.KnownInstances.fromIds.ROOT).to.equal(Replacement)
			expect(Fixture.KnownInstances.fromInstances[Fixture.Root]).to.equal(nil)
			expect(Replacement.Parent).to.equal(container)
			expect(Replacement.Source).to.equal("print('replacement source')")
			expect(Fixture.Child.Parent).to.equal(Replacement)
			expect(Fixture.KnownInstances.fromIds.CHILD).to.equal(Fixture.Child)
			expect(Fixture.Root.Parent).to.equal(nil)
		end)

		Fixture.KnownInstances:stop()
		Fixture.Root:Destroy()
	end)

	for _, Outcome in { false, "error" } do
		it(`should retain the original script after delayed source initialization fails with {Outcome}`, function()
			local Fixture = NewScriptReplacement()

			WithPendingSourceInitialization(Fixture, function(PatchThread, Replacement)
				local Completed, Unapplied = coroutine.resume(PatchThread, Outcome)
				assert(Completed, tostring(Unapplied))
				expect(coroutine.status(PatchThread)).to.equal("dead")
				expect(#Unapplied.updated).to.equal(1)
				expect(Unapplied.updated[1]).to.equal(Fixture.Patch.updated[1])
				expect(next(Unapplied.added)).to.equal(nil)
				expect(next(Unapplied.removed)).to.equal(nil)
				expect(Fixture.KnownInstances.fromIds.ROOT).to.equal(Fixture.Root)
				expect(Fixture.KnownInstances.fromInstances[Fixture.Root]).to.equal("ROOT")
				expect(Fixture.Root.Parent).to.equal(container)
				expect(Fixture.Root.Source).to.equal(Fixture.Source)
				expect(Fixture.Child.Parent).to.equal(Fixture.Root)
				expect(Fixture.KnownInstances.fromIds.CHILD).to.equal(Fixture.Child)

				local Reparented = pcall(function()
					Replacement.Parent = container
				end)
				expect(Reparented).to.equal(false)
				expect(#container:GetChildren()).to.equal(1)
			end)

			Fixture.KnownInstances:stop()
		end)
	end

	for _, Invalidation in { "stop", "remove", "supersede" } do
		it(`should not publish delayed source initialization after map {Invalidation}`, function()
			local Fixture = NewScriptReplacement()

			WithPendingSourceInitialization(Fixture, function(PatchThread, Replacement)
				local ExpectedInstance
				if Invalidation == "stop" then
					Fixture.KnownInstances:stop()
				elseif Invalidation == "remove" then
					Fixture.KnownInstances:removeId("ROOT")
				else
					ExpectedInstance = Instance.new("Folder")
					ExpectedInstance.Parent = container
					Fixture.KnownInstances:insert("ROOT", ExpectedInstance)
				end

				local Completed, Unapplied = coroutine.resume(PatchThread, true)
				assert(Completed, tostring(Unapplied))
				expect(coroutine.status(PatchThread)).to.equal("dead")
				expect(#Unapplied.updated).to.equal(1)
				expect(Unapplied.updated[1]).to.equal(Fixture.Patch.updated[1])
				expect(next(Unapplied.added)).to.equal(nil)
				expect(Fixture.KnownInstances.fromIds.ROOT).to.equal(ExpectedInstance)
				expect(Fixture.KnownInstances.fromInstances[Fixture.Root]).to.equal(nil)
				expect(Fixture.Root.Parent).to.equal(container)
				expect(Fixture.Root.Source).to.equal(Fixture.Source)
				expect(Fixture.Child.Parent).to.equal(Fixture.Root)

				local Reparented = pcall(function()
					Replacement.Parent = container
				end)
				expect(Reparented).to.equal(false)
			end)

			Fixture.KnownInstances:stop()
		end)
	end

	for _, Failure in
		{
			{ ClassName = "ModuleScript", Scriptability = "None", Description = "read" },
			{ ClassName = "Script", Scriptability = "Read", Description = "written" },
		}
	do
		it(`should retain the original script and children when source cannot be {Failure.Description}`, function()
			local Source = "return 'keep me'"
			local Root = Instance.new("ModuleScript")
			Root.Source = Source
			Root.Parent = container

			local Child = Instance.new("Folder")
			Child.Parent = Root

			local KnownInstances = InstanceMap.new()
			KnownInstances:insert("ROOT", Root)
			KnownInstances:insert("CHILD", Child)

			local Patch = PatchSet.newEmpty()
			table.insert(Patch.updated, {
				id = "ROOT",
				changedClassName = "Script",
			})

			local Unapplied = WithSourceScriptability(Failure.ClassName, Failure.Scriptability, function()
				return applyPatch(KnownInstances, Patch)
			end)

			expect(#Unapplied.updated).to.equal(1)
			expect(Unapplied.updated[1]).to.equal(Patch.updated[1])
			expect(next(Unapplied.added)).to.equal(nil)
			expect(next(Unapplied.removed)).to.equal(nil)
			expect(KnownInstances.fromIds.ROOT).to.equal(Root)
			expect(KnownInstances.fromInstances[Root]).to.equal("ROOT")
			expect(KnownInstances.fromIds.CHILD).to.equal(Child)
			expect(Root.Parent).to.equal(container)
			expect(Root.Source).to.equal(Source)
			expect(Child.Parent).to.equal(Root)
			expect(#container:GetChildren()).to.equal(1)

			KnownInstances:stop()
		end)
	end
end
