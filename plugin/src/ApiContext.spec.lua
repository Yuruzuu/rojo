return function()
	local Packages = script.Parent.Parent.Packages
	local ApiContext = require(script.Parent.ApiContext)
	local Config = require(script.Parent.Config)
	local Http = require(Packages.Http)
	local Promise = require(Packages.Promise)

	local function Deferred()
		local Resolve, Reject

		local Request = Promise.new(function(ResolveRequest, RejectRequest)
			Resolve = ResolveRequest
			Reject = RejectRequest
		end)

		return Request, Resolve, Reject
	end

	local function WithHttp(Request, Decode, Callback)
		local OriginalGet = Http.get
		local OriginalPost = Http.post
		local OriginalDecode = Http.Response.msgpack

		Http.get = Request
		Http.post = Request
		Http.Response.msgpack = Decode or function(Response)
			return Response.body
		end

		local Success, Error = xpcall(Callback, debug.traceback)

		Http.get = OriginalGet
		Http.post = OriginalPost
		Http.Response.msgpack = OriginalDecode

		assert(Success, Error)
	end

	local function NewSignal()
		local Signal = { Callbacks = {}, Connections = {} }

		function Signal:Connect(Callback)
			local Connection = { Connected = true }

			function Connection.Disconnect()
				Connection.Connected = false
			end

			table.insert(self.Callbacks, Callback)
			table.insert(self.Connections, Connection)

			return Connection
		end

		return Signal
	end

	local function NewSocket()
		local Socket = {
			MessageReceived = NewSignal(),
			Closed = NewSignal(),
			Error = NewSignal(),
			CloseCount = 0,
		}

		function Socket:Close()
			self.CloseCount += 1
			for Index, Callback in self.Closed.Callbacks do
				if self.Closed.Connections[Index].Connected then
					Callback()
				end
			end
		end

		return Socket
	end

	describe("request lifecycle", function()
		it("cancels every pending HTTP operation on disconnect", function()
			local Pending = {}

			WithHttp(
				function()
					local Request, Resolve = Deferred()
					table.insert(Pending, { Request = Request, Resolve = Resolve })

					return Request
				end,
				nil,
				function()
					local Context = ApiContext.new("http://localhost")
					local Patch = { added = {}, updated = {}, removed = {} }

					local Requests = {
						Context:connect(),
						Context:read({ "root" }),
						Context:write(Patch),
						Context:open("script"),
						Context:serialize({ "root" }),
						Context:refPatch({ "root" }),
					}

					local ActiveCount = 0
					for _ in Context.__activeRequests do
						ActiveCount += 1
					end
					expect(ActiveCount).to.equal(#Requests)

					Context:disconnect()

					for Index, Request in Requests do
						expect(Request:getStatus()).to.equal(Promise.Status.Cancelled)
						expect(Pending[Index].Request:getStatus()).to.equal(Promise.Status.Cancelled)
						Pending[Index].Resolve({ code = 200, body = {} })
					end
					expect(next(Context.__activeRequests)).to.equal(nil)
					expect(Context.__sessionId).to.equal(nil)
				end
			)
		end)

		it("releases completed and rejected requests from context ownership", function()
			local Pending, Resolve = Deferred()

			WithHttp(
				function()
					return Pending
				end,
				nil,
				function()
					local Context = ApiContext.new("http://localhost")
					Context.__sessionId = "session"

					local Request = Context:read({ "root" })
					Resolve({ code = 200, body = { sessionId = "session", messageCursor = 1, instances = {} } })

					expect(Request:await()).to.equal(true)
					expect(next(Context.__activeRequests)).to.equal(nil)

					local Reject
					Pending, Resolve, Reject = Deferred()
					Request = Context:read({ "root" })
					Request:catch(function() end)
					Reject("offline")

					expect(Request:await()).to.equal(false)
					expect(next(Context.__activeRequests)).to.equal(nil)
				end
			)
		end)

		it("rejects new operations and cursor changes after disconnect", function()
			local RequestCount = 0

			WithHttp(
				function()
					RequestCount += 1
					return Promise.resolve({ code = 200, body = {} })
				end,
				nil,
				function()
					local Context = ApiContext.new("http://localhost")
					Context:disconnect()

					local Requests = {
						Context:connect(),
						Context:read({ "root" }),
						Context:write({ added = {}, updated = {}, removed = {} }),
						Context:open("script"),
						Context:serialize({ "root" }),
						Context:refPatch({ "root" }),
						Context:connectWebSocket({}),
					}

					for _, Request in Requests do
						expect(Request:await()).to.equal(false)
					end
					Context:setMessageCursor(10)

					expect(RequestCount).to.equal(0)
					expect(Context.__messageCursor).to.equal(-1)
				end
			)
		end)

		it("ignores a connect decoder already running when disconnected", function()
			local DecodeThread

			WithHttp(function()
				return Promise.resolve({ code = 200 })
			end, function()
				DecodeThread = coroutine.running()

				return coroutine.yield()
			end, function()
				local Context = ApiContext.new("http://localhost")
				local Request = Context:connect()
				Context:disconnect()

				if coroutine.status(DecodeThread) ~= "dead" then
					assert(coroutine.resume(DecodeThread, {
						sessionId = "late-session",
						protocolVersion = Config.protocolVersion,
						serverVersion = "test",
						rootInstanceId = "root",
					}))
				end

				expect(Request:getStatus()).to.equal(Promise.Status.Cancelled)
				expect(Context.__sessionId).to.equal(nil)
				expect(Context.__messageCursor).to.equal(-1)
				expect(next(Context.__activeRequests)).to.equal(nil)
			end)
		end)
	end)

	describe("WebSocket lifecycle", function()
		it("closes a socket whose creation finishes after disconnect", function()
			local Context = ApiContext.new("http://localhost")
			local Socket = NewSocket()

			local CreationThread
			Context.__CreateWebSocketClient = function()
				CreationThread = coroutine.running()

				return coroutine.yield()
			end

			local Request = Context:connectWebSocket({})
			Context:disconnect()
			expect(coroutine.status(CreationThread)).to.equal("suspended")
			assert(coroutine.resume(CreationThread, Socket))
			Context:disconnect()

			expect(Request:getStatus()).to.equal(Promise.Status.Cancelled)
			expect(Socket.CloseCount).to.equal(1)
			expect(#Socket.MessageReceived.Callbacks).to.equal(0)
			expect(Context.__wsClient).to.equal(nil)
		end)

		it("cleans up a failed socket once and ignores queued socket callbacks", function()
			local Context = ApiContext.new("http://localhost")
			Context.__sessionId = "session"

			local Socket = NewSocket()
			Context.__CreateWebSocketClient = function()
				return Socket
			end

			local PacketCount = 0
			local Request = Context:connectWebSocket({
				messages = function()
					PacketCount += 1
				end,
			})
			Request:catch(function() end)

			Socket.Error.Callbacks[1]("500", "failed")
			Socket.Closed.Callbacks[1]()
			Socket.MessageReceived.Callbacks[1](Http.msgpackEncode({
				sessionId = "session",
				packetType = "messages",
				body = { messages = {}, messageCursor = 1 },
			}))
			Context:disconnect()
			Context:disconnect()

			expect(Request:await()).to.equal(false)
			expect(Socket.CloseCount).to.equal(1)
			expect(PacketCount).to.equal(0)
			expect(Context.__wsClient).to.equal(nil)
			expect(next(Context.__activeRequests)).to.equal(nil)
			for _, Signal in { Socket.Closed, Socket.Error, Socket.MessageReceived } do
				expect(Signal.Connections[1].Connected).to.equal(false)
			end
		end)
	end)
end
