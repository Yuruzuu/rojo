return function()
	local Packages = script.Parent.Parent.Packages

	local Promise = require(Packages.Promise)

	local PatchSet = require(script.Parent.PatchSet)
	local ServeSession = require(script.Parent.ServeSession)

	local function Deferred()
		local Resolve

		local Request = Promise.new(function(ResolveRequest)
			Resolve = ResolveRequest
		end)

		return Request, Resolve
	end

	local function NewSession()
		local Calls = {
			Read = 0,
			Write = 0,
			Hydrate = 0,
			Diff = 0,
			Patch = 0,
			Socket = 0,
			Cursor = 0,
			Place = 0,
			Serialize = 0,
			RefPatch = 0,
			Insert = 0,
			Disconnect = 0,
			StopMap = 0,
			StopBatcher = 0,
			Loading = {},
			Statuses = {},
			CursorValues = {},
		}

		local ServerInfo = { projectName = "test", rootInstanceId = "root" }
		local ReadResponse = { messageCursor = 1, instances = {} }

		local Context = {
			connect = function()
				return Promise.resolve(ServerInfo)
			end,
			read = function()
				Calls.Read += 1

				return Promise.resolve(ReadResponse)
			end,
			write = function()
				Calls.Write += 1

				return Promise.resolve()
			end,
			connectWebSocket = function(_, Handlers)
				Calls.Socket += 1
				Calls.Handlers = Handlers

				return Promise.new(function() end)
			end,
			setMessageCursor = function(_, Cursor)
				Calls.Cursor += 1

				Calls.LastCursor = Cursor
				table.insert(Calls.CursorValues, Cursor)
			end,
			disconnect = function()
				Calls.Disconnect += 1
			end,
		}

		local Session = setmetatable({
			__status = ServeSession.Status.NotStarted,
			__apiContext = Context,
			__twoWaySync = true,
			__connections = {},
			__precommitCallbacks = {},
			__postcommitCallbacks = {},
			__IncomingPackets = {},
			__IncomingPacketTail = 0,
			__ProcessingIncomingPackets = false,
			__statusChangedCallback = function(Status)
				table.insert(Calls.Statuses, Status)
			end,
			__updateLoadingText = function(Text)
				table.insert(Calls.Loading, Text)
			end,
			__instanceMap = {
				fromIds = {},
				fromInstances = {},
				insert = function()
					Calls.Insert += 1
				end,
				stop = function()
					Calls.StopMap += 1
				end,
			},
			__changeBatcher = {
				stop = function()
					Calls.StopBatcher += 1
				end,
			},
			__reconciler = {
				hydrate = function()
					Calls.Hydrate += 1
				end,
				diff = function()
					Calls.Diff += 1

					return true, PatchSet.newEmpty()
				end,
			},
			__applyPatch = function()
				Calls.Patch += 1
			end,
			__applyGameAndPlaceId = function()
				Calls.Place += 1
			end,
		}, ServeSession)

		return Session, Calls, ServerInfo, ReadResponse
	end

	local function NewPacketSession()
		local Session, Calls = NewSession()
		Session:start()

		Calls.Patch = 0
		Calls.Applied = {}

		local Original = {
			Name = "part",
			Source = "original",
			GetChildren = function()
				return {}
			end,
		}
		local Parent = {
			GetChildren = function()
				return { Original }
			end,
		}
		Original.Parent = Parent

		local Replacement = {
			Name = "part",
			Source = "older",
			GetChildren = function()
				return {}
			end,
		}
		Session.__instanceMap.fromIds.part = Original
		Session.__instanceMap.fromInstances[Original] = "part"
		Session.__instanceMap.insert = function(Map, Id, Instance)
			Calls.Insert += 1

			local Previous = Map.fromIds[Id]
			Map.fromInstances[Previous] = nil
			Map.fromIds[Id] = Instance
			Map.fromInstances[Instance] = Id
		end

		local Request, Resolve = Deferred()
		Session.__apiContext.serialize = function()
			Calls.Serialize += 1

			return Request
		end
		Session.__DeserializeInstances = function()
			return {
				{
					GetChildren = function()
						return { { Name = "part", Value = Replacement } }
					end,
				},
			}
		end
		Session.__apiContext.refPatch = function()
			Calls.RefPatch += 1

			return Promise.resolve({ patch = PatchSet.newEmpty() })
		end
		Session.__reconciler.applyPatch = function()
			return PatchSet.newEmpty()
		end

		local OlderPatch = PatchSet.newEmpty()
		local FollowingPatch = PatchSet.newEmpty()
		local NewerPatch = PatchSet.newEmpty()
		Session.__applyPatch = function(self, Patch)
			Calls.Patch += 1

			if Patch == OlderPatch then
				local Replaced = self:__replaceInstances({ "part" })
				if not Replaced then
					return
				end
			else
				self.__instanceMap.fromIds.part.Source = if Patch == FollowingPatch then "following" else "newer"
			end

			table.insert(Calls.Applied, {
				Source = self.__instanceMap.fromIds.part.Source,
				Cursor = Calls.LastCursor,
			})
		end

		return Session, Calls, Resolve, Original, Replacement, OlderPatch, FollowingPatch, NewerPatch
	end

	describe("initial sync lifecycle", function()
		it("ignores connect completion after stop", function()
			local Session, Calls, ServerInfo = NewSession()
			local Request, Resolve = Deferred()
			Session.__apiContext.connect = function()
				return Request
			end

			Session:start()
			Session:stop()

			local LoadingCount = #Calls.Loading
			Resolve(ServerInfo)

			expect(Session:getStatus()).to.equal(ServeSession.Status.Disconnected)
			expect(Calls.Read).to.equal(0)
			expect(Calls.Place).to.equal(0)
			expect(Calls.Socket).to.equal(0)
			expect(#Calls.Loading).to.equal(LoadingCount)
			expect(#Calls.Statuses).to.equal(2)
		end)

		it("ignores initial read completion after stop", function()
			local Session, Calls, _, ReadResponse = NewSession()
			local Request, Resolve = Deferred()
			Session.__apiContext.read = function()
				return Request
			end

			Session:start()
			Session:stop()
			Resolve(ReadResponse)

			expect(Calls.Cursor).to.equal(0)
			expect(Calls.Hydrate).to.equal(0)
			expect(Calls.Diff).to.equal(0)
			expect(Calls.Patch).to.equal(0)
			expect(Calls.Socket).to.equal(0)
		end)

		for _, Decision in { "Accept", "Reject" } do
			it(`ignores {Decision} confirmation resumed after stop`, function()
				local Session, Calls = NewSession()

				local ConfirmationThread
				Session:setConfirmCallback(function()
					ConfirmationThread = coroutine.running()

					return coroutine.yield()
				end)

				Session:start()
				Session:stop()
				assert(coroutine.resume(ConfirmationThread, Decision))

				expect(Calls.Patch).to.equal(0)
				expect(Calls.Write).to.equal(0)
				expect(Calls.Place).to.equal(0)
				expect(Calls.Socket).to.equal(0)
				expect(Session:getStatus()).to.equal(ServeSession.Status.Disconnected)
			end)
		end

		for _, Boundary in { "hydrate", "diff" } do
			it(`stops initial sync after a suspended {Boundary} returns`, function()
				local Session, Calls = NewSession()

				local BoundaryThread
				Session.__reconciler[Boundary] = function()
					BoundaryThread = coroutine.running()
					coroutine.yield()

					return true, PatchSet.newEmpty()
				end

				Session:start()
				Session:stop()
				assert(coroutine.resume(BoundaryThread))

				expect(Calls.Patch).to.equal(0)
				expect(Calls.Socket).to.equal(0)
				if Boundary == "hydrate" then
					expect(Calls.Diff).to.equal(0)
				end
			end)
		end

		it("does not open a socket when the connected status callback stops the session", function()
			local Session, Calls = NewSession()
			Session:onStatusChanged(function(Status)
				if Status == ServeSession.Status.Connected then
					Session:stop()
				end
			end)

			Session:start()

			expect(Session:getStatus()).to.equal(ServeSession.Status.Disconnected)
			expect(Calls.Place).to.equal(0)
			expect(Calls.Socket).to.equal(0)
		end)

		it("stops before reading when a loading callback disconnects", function()
			local Session, Calls = NewSession()
			Session:setUpdateLoadingTextCallback(function(Text)
				if Text == "Loading initial data from server..." then
					Session:stop()
				end
			end)

			Session:start()

			expect(Calls.Read).to.equal(0)
			expect(Calls.Socket).to.equal(0)
		end)
	end)

	describe("fallback lifecycle", function()
		it("does not deserialize or request refs after a late serialize response", function()
			local Session, Calls = NewSession()
			Session.__status = ServeSession.Status.Connected

			local Request, Resolve = Deferred()
			Session.__apiContext.serialize = function()
				return Request
			end
			Session.__apiContext.refPatch = function()
				Calls.RefPatch += 1

				return Promise.resolve({ patch = PatchSet.newEmpty() })
			end
			Session.__DeserializeInstances = function()
				error("Stopped session must not deserialize")
			end

			local Replacement = Promise.new(function(Complete)
				Complete(Session:__replaceInstances({ "part" }))
			end)
			Session:stop()
			Resolve({ modelContents = "unused" })

			local Completed, Replaced = Replacement:await()
			expect(Completed).to.equal(true)
			expect(Replaced).to.equal(false)
			expect(Calls.RefPatch).to.equal(0)
			expect(Calls.Insert).to.equal(0)
		end)

		it("does not request refs after deserialization resumes after stop", function()
			local Session, Calls = NewSession()
			Session.__status = ServeSession.Status.Connected
			Session.__apiContext.serialize = function()
				return Promise.resolve({ modelContents = "unused" })
			end
			Session.__apiContext.refPatch = function()
				Calls.RefPatch += 1

				return Promise.resolve({ patch = PatchSet.newEmpty() })
			end

			local DeserializeThread
			Session.__DeserializeInstances = function()
				DeserializeThread = coroutine.running()

				return coroutine.yield()
			end

			local Replacement = Promise.new(function(Complete)
				Complete(Session:__replaceInstances({ "part" }))
			end)
			Session:stop()
			assert(coroutine.resume(DeserializeThread, {}))

			local Completed, Replaced = Replacement:await()
			expect(Completed).to.equal(true)
			expect(Replaced).to.equal(false)
			expect(Calls.RefPatch).to.equal(0)
			expect(Calls.Insert).to.equal(0)
		end)

		it("does not swap instances after a late ref patch response", function()
			local Session, Calls = NewSession()
			Session.__status = ServeSession.Status.Connected

			local Original = { Parent = "original-parent" }
			local NewInstance = { Parent = nil }
			Session.__instanceMap.fromIds.part = Original
			Session.__apiContext.serialize = function()
				return Promise.resolve({ modelContents = "unused" })
			end
			Session.__DeserializeInstances = function()
				return {
					{
						GetChildren = function()
							return { { Name = "part", Value = NewInstance } }
						end,
					},
				}
			end

			local Request, Resolve = Deferred()
			Session.__apiContext.refPatch = function()
				Calls.RefPatch += 1

				return Request
			end

			local Replacement = Promise.new(function(Complete)
				Complete(Session:__replaceInstances({ "part" }))
			end)
			Session:stop()
			Resolve({ patch = PatchSet.newEmpty() })

			local Completed, Replaced = Replacement:await()
			expect(Completed).to.equal(true)
			expect(Replaced).to.equal(false)
			expect(Calls.RefPatch).to.equal(1)
			expect(Calls.Insert).to.equal(0)
			expect(Original.Parent).to.equal("original-parent")
			expect(NewInstance.Parent).to.equal(nil)
		end)
	end)

	describe("running session lifecycle", function()
		it("finishes a suspended fallback packet and its cursor before applying the next packet", function()
			local Session, Calls, Resolve, Original, Replacement, OlderPatch, FollowingPatch, NewerPatch =
				NewPacketSession()

			local FirstPacket = Promise.new(function(Complete)
				Calls.Handlers.messages({ messages = { OlderPatch, FollowingPatch }, messageCursor = 10 })
				Complete()
			end)
			Calls.Handlers.messages({ messages = { NewerPatch }, messageCursor = 11 })

			expect(FirstPacket:getStatus()).to.equal(Promise.Status.Started)
			expect(Calls.Patch).to.equal(1)
			expect(Calls.Serialize).to.equal(1)
			expect(Calls.LastCursor).to.equal(1)
			expect(Session.__instanceMap.fromIds.part).to.equal(Original)
			expect(Original.Source).to.equal("original")
			expect(Replacement.Parent).to.equal(nil)

			Resolve({ modelContents = "older-model" })
			expect(FirstPacket:await()).to.equal(true)

			expect(Calls.Patch).to.equal(3)
			expect(Calls.Applied[1].Source).to.equal("older")
			expect(Calls.Applied[2].Source).to.equal("following")
			expect(Calls.Applied[3].Source).to.equal("newer")
			expect(Calls.Applied[3].Cursor).to.equal(10)
			expect(Session.__instanceMap.fromIds.part).to.equal(Replacement)
			expect(Replacement.Source).to.equal("newer")
			expect(Original.Parent).to.equal(nil)
			expect(#Calls.CursorValues).to.equal(3)
			expect(Calls.CursorValues[2]).to.equal(10)
			expect(Calls.CursorValues[3]).to.equal(11)
			expect(Calls.LastCursor).to.equal(11)
			expect(Session.__ProcessingIncomingPackets).to.equal(false)
			expect(next(Session.__IncomingPackets)).to.equal(nil)
		end)

		it("drops queued packets on stop while fallback is pending without advancing the cursor", function()
			local Session, Calls, Resolve, Original, Replacement, OlderPatch, FollowingPatch, NewerPatch =
				NewPacketSession()

			local FirstPacket = Promise.new(function(Complete)
				Calls.Handlers.messages({ messages = { OlderPatch, FollowingPatch }, messageCursor = 10 })
				Complete()
			end)
			Calls.Handlers.messages({ messages = { NewerPatch }, messageCursor = 11 })
			Session:stop()

			expect(next(Session.__IncomingPackets)).to.equal(nil)
			expect(Session.__ProcessingIncomingPackets).to.equal(false)

			Resolve({ modelContents = "older-model" })
			expect(FirstPacket:await()).to.equal(true)

			expect(Session:getStatus()).to.equal(ServeSession.Status.Disconnected)
			expect(Calls.Patch).to.equal(1)
			expect(#Calls.Applied).to.equal(0)
			expect(Calls.RefPatch).to.equal(0)
			expect(Calls.Insert).to.equal(0)
			expect(#Calls.CursorValues).to.equal(1)
			expect(Calls.LastCursor).to.equal(1)
			expect(Original.Source).to.equal("original")
			expect(Replacement.Parent).to.equal(nil)
		end)

		it("stops a partially applied session and clears the packet queue after an exception", function()
			local Session, Calls, Resolve, Original, Replacement, OlderPatch, FollowingPatch, NewerPatch =
				NewPacketSession()
			Session.__reconciler.applyPatch = function()
				error("reference patch failed")
			end

			local Failure
			Session:onStatusChanged(function(_, Detail)
				Failure = Detail
			end)

			local FirstPacket = Promise.new(function(Complete)
				Calls.Handlers.messages({ messages = { OlderPatch, FollowingPatch }, messageCursor = 10 })
				Complete()
			end)
			Calls.Handlers.messages({ messages = { NewerPatch }, messageCursor = 11 })

			Resolve({ modelContents = "older-model" })
			expect(FirstPacket:await()).to.equal(true)

			expect(Session:getStatus()).to.equal(ServeSession.Status.Disconnected)
			expect(string.find(Failure, "reference patch failed", 1, true)).to.be.ok()
			expect(Calls.Disconnect).to.equal(1)
			expect(Calls.Patch).to.equal(1)
			expect(Session.__instanceMap.fromIds.part).to.equal(Replacement)
			expect(Replacement.Source).to.equal("older")
			expect(Original.Parent).to.equal(nil)
			expect(#Calls.CursorValues).to.equal(1)
			expect(Calls.LastCursor).to.equal(1)
			expect(next(Session.__IncomingPackets)).to.equal(nil)
			expect(Session.__IncomingPacketTail).to.equal(0)
			expect(Session.__ProcessingIncomingPackets).to.equal(false)
		end)

		it("does not apply another message or advance the cursor after a suspended patch stops", function()
			local Session, Calls = NewSession()
			Session:start()

			local CursorCount = Calls.Cursor
			local PatchCount = 0
			Session.__applyPatch = function()
				PatchCount += 1
				coroutine.yield()
			end

			local MessageThread = coroutine.create(function()
				Calls.Handlers.messages({
					messages = { PatchSet.newEmpty(), PatchSet.newEmpty() },
					messageCursor = 10,
				})
			end)
			assert(coroutine.resume(MessageThread))
			Session:stop()
			assert(coroutine.resume(MessageThread))

			expect(PatchCount).to.equal(1)
			expect(Calls.Cursor).to.equal(CursorCount)
			expect(Session:getStatus()).to.equal(ServeSession.Status.Disconnected)
		end)

		it("makes repeated stop and attempted restart inert", function()
			local Session, Calls = NewSession()
			Session:stop()
			Session:stop()
			Session:start()
			Session:setLoadingText("late callback")

			expect(Calls.Disconnect).to.equal(1)
			expect(Calls.StopMap).to.equal(1)
			expect(Calls.StopBatcher).to.equal(1)
			expect(#Calls.Statuses).to.equal(1)
			expect(#Calls.Loading).to.equal(0)
		end)
	end)
end
