-- Services
local HttpService = game:GetService("HttpService")

local Packages = script.Parent.Parent.Packages
local Http = require(Packages.Http)
local Log = require(Packages.Log)
local Promise = require(Packages.Promise)

local Config = require(script.Parent.Config)
local Types = require(script.Parent.Types)
local Version = require(script.Parent.Version)

local validateApiInfo = Types.ifEnabled(Types.ApiInfoResponse)
local validateApiRead = Types.ifEnabled(Types.ApiReadResponse)
local validateApiSocketPacket = Types.ifEnabled(Types.ApiSocketPacket)
local validateApiSerialize = Types.ifEnabled(Types.ApiSerializeResponse)
local validateApiRefPatch = Types.ifEnabled(Types.ApiRefPatchResponse)

local function rejectFailedRequests(response)
	if response.code >= 400 then
		local message = string.format("HTTP %s:\n%s", tostring(response.code), response.body)

		return Promise.reject(message)
	end

	return response
end

local function rejectWrongProtocolVersion(infoResponseBody)
	if infoResponseBody.protocolVersion ~= Config.protocolVersion then
		local message = (
			"Found a Rojo dev server, but it's using a different protocol version, and is incompatible."
			.. "\nMake sure you have matching versions of both the Rojo plugin and server!"
			.. "\n\nYour client is version %s, with protocol version %s. It expects server version %s."
			.. "\nYour server is version %s, with protocol version %s."
			.. "\n\nGo to https://github.com/rojo-rbx/rojo for more details."
		):format(
			Version.display(Config.version),
			Config.protocolVersion,
			Config.expectedServerVersionString,
			infoResponseBody.serverVersion,
			infoResponseBody.protocolVersion
		)

		return Promise.reject(message)
	end

	return Promise.resolve(infoResponseBody)
end

local function rejectWrongPlaceId(infoResponseBody)
	if infoResponseBody.expectedPlaceIds ~= nil then
		local foundId = table.find(infoResponseBody.expectedPlaceIds, game.PlaceId)

		if not foundId then
			local idList = {}
			for _, id in ipairs(infoResponseBody.expectedPlaceIds) do
				table.insert(idList, "- " .. tostring(id))
			end

			local message = (
				"Found a Rojo server, but its project is set to only be used with a specific list of places."
				.. "\nYour place ID is %u, but needs to be one of these:"
				.. "\n%s"
				.. "\n\nTo change this list, edit 'servePlaceIds' in your .project.json file."
			):format(game.PlaceId, table.concat(idList, "\n"))

			return Promise.reject(message)
		end
	end

	if infoResponseBody.unexpectedPlaceIds ~= nil then
		local foundId = table.find(infoResponseBody.unexpectedPlaceIds, game.PlaceId)

		if foundId then
			local idList = {}
			for _, id in ipairs(infoResponseBody.unexpectedPlaceIds) do
				table.insert(idList, "- " .. tostring(id))
			end

			local message = (
				"Found a Rojo server, but its project is set to not be used with a specific list of places."
				.. "\nYour place ID is %u, but needs to not be one of these:"
				.. "\n%s"
				.. "\n\nTo change this list, edit 'blockedPlaceIds' in your .project.json file."
			):format(game.PlaceId, table.concat(idList, "\n"))

			return Promise.reject(message)
		end
	end

	return Promise.resolve(infoResponseBody)
end

local ApiContext = {}
ApiContext.__index = ApiContext

function ApiContext.new(baseUrl)
	assert(type(baseUrl) == "string", "baseUrl must be a string")

	local self = {
		__baseUrl = baseUrl,
		__sessionId = nil,
		__messageCursor = -1,
		__wsClient = nil,
		__connected = true,
		__activeRequests = {},
	}

	return setmetatable(self, ApiContext)
end

function ApiContext:__fmtDebug(output)
	output:writeLine("ApiContext {{")
	output:indent()

	output:writeLine("Connected: {}", self.__connected)
	output:writeLine("Base URL: {}", self.__baseUrl)
	output:writeLine("Session ID: {}", self.__sessionId)
	output:writeLine("Message Cursor: {}", self.__messageCursor)

	output:unindent()
	output:write("}")
end

function ApiContext:disconnect()
	self.__connected = false

	local ActiveRequests = self.__activeRequests
	self.__activeRequests = {}
	for Request in ActiveRequests do
		Log.trace("Cancelling request {}", Request)
		Request:cancel()
	end

	if self.__wsClient then
		Log.trace("Closing WebSocket client")
		local WebSocketClient = self.__wsClient
		self.__wsClient = nil
		WebSocketClient:Close()
	end
end

function ApiContext:__TrackRequest(Request)
	self.__activeRequests[Request] = true
	if not self.__connected then
		Request:cancel()
	end

	return Request:finally(function()
		self.__activeRequests[Request] = nil
	end)
end

function ApiContext:__Request(Method, Url, Body)
	if not self.__connected then
		return Promise.reject("API context disconnected")
	end

	return self:__TrackRequest(
		Http[Method](Url, Body):andThen(rejectFailedRequests):andThen(Http.Response.msgpack):andThen(function(Response)
			-- HTTP runs in a detached coroutine, so cancellation cannot stop the request itself.
			if not self.__connected then
				return Promise.reject("API context disconnected")
			end

			return Response
		end)
	)
end

function ApiContext:setMessageCursor(index)
	if not self.__connected then
		return
	end

	self.__messageCursor = index
end

function ApiContext:connect()
	local url = ("%s/api/rojo"):format(self.__baseUrl)

	return self:__Request("get", url)
		:andThen(rejectWrongProtocolVersion)
		:andThen(function(body)
			assert(validateApiInfo(body))

			return body
		end)
		:andThen(rejectWrongPlaceId)
		:andThen(function(body)
			if not self.__connected then
				return Promise.reject("API context disconnected")
			end

			self.__sessionId = body.sessionId

			return body
		end)
end

function ApiContext:read(ids)
	local url = ("%s/api/read/%s"):format(self.__baseUrl, table.concat(ids, ","))

	return self:__Request("get", url):andThen(function(body)
		if body.sessionId ~= self.__sessionId then
			return Promise.reject("Server changed ID")
		end

		assert(validateApiRead(body))

		return body
	end)
end

function ApiContext:write(patch)
	local url = ("%s/api/write"):format(self.__baseUrl)

	local updated = {}
	for _, update in ipairs(patch.updated) do
		local fixedUpdate = {
			id = update.id,
			changedName = update.changedName,
		}

		if next(update.changedProperties) ~= nil then
			fixedUpdate.changedProperties = update.changedProperties
		end

		table.insert(updated, fixedUpdate)
	end

	-- Only add the 'added' field if the table is non-empty, or else the msgpack
	-- encode implementation will turn the table into an array instead of a map,
	-- causing API validation to fail.
	local added
	if next(patch.added) ~= nil then
		added = patch.added
	end

	local body = {
		sessionId = self.__sessionId,
		removed = patch.removed,
		updated = updated,
		added = added,
	}

	body = Http.msgpackEncode(body)

	return self:__Request("post", url, body):andThen(function(responseBody)
		Log.info("Write response: {:?}", responseBody)

		return responseBody
	end)
end

function ApiContext:connectWebSocket(packetHandlers)
	if not self.__connected then
		return Promise.reject("API context disconnected")
	end

	local url = ("%s/api/socket/%s"):format(self.__baseUrl, self.__messageCursor)
	-- Convert HTTP/HTTPS URL to WS/WSS
	url = url:gsub("^http://", "ws://"):gsub("^https://", "wss://")

	return self:__TrackRequest(Promise.new(function(resolve, reject, OnCancel)
		local Cancelled = false
		local WebSocketClient
		local Connections = {}
		local Cleaned = false

		local function Cleanup(CloseClient)
			if Cleaned or not WebSocketClient then
				return
			end

			Cleaned = true
			for _, Connection in Connections do
				Connection:Disconnect()
			end

			if self.__wsClient == WebSocketClient then
				self.__wsClient = nil
			end

			if CloseClient then
				WebSocketClient:Close()
			end
		end

		OnCancel(function()
			Cancelled = true
			Cleanup(true)
		end)

		local function CreateAndConnect()
			local success, wsClient = pcall(self.__CreateWebSocketClient, self, url)
			if success then
				WebSocketClient = wsClient
			end

			if Cancelled or not self.__connected then
				Cleanup(true)
				reject("API context disconnected")
				return
			end

			if not success then
				reject("Failed to create WebSocket client: " .. tostring(wsClient))
				return
			end
			self.__wsClient = wsClient

			table.insert(
				Connections,
				wsClient.MessageReceived:Connect(function(msg)
					if not self.__connected or self.__wsClient ~= wsClient then
						return
					end

					local data = Http.msgpackDecode(msg)
					if not self.__connected or self.__wsClient ~= wsClient then
						return
					end

					if data.sessionId ~= self.__sessionId then
						Log.warn("Received message with wrong session ID; ignoring")
						return
					end

					assert(validateApiSocketPacket(data))

					Log.trace("Received websocket packet: {:#?}", data)

					local handler = packetHandlers[data.packetType]
					if handler then
						local ok, err = pcall(handler, data.body)
						if not ok then
							Log.error("Error in WebSocket packet handler for type '%s': %s", data.packetType, err)
						end
					else
						Log.warn("No handler for WebSocket packet type '%s'", data.packetType)
					end
				end)
			)

			table.insert(
				Connections,
				wsClient.Closed:Connect(function()
					if Cleaned then
						return
					end

					Cleanup(false)

					if self.__connected then
						reject("WebSocket connection closed unexpectedly")
					else
						resolve()
					end
				end)
			)

			table.insert(
				Connections,
				wsClient.Error:Connect(function(code, msg)
					if Cleaned then
						return
					end

					Cleanup(true)

					reject("WebSocket error: " .. code .. " - " .. msg)
				end)
			)
		end

		-- Cancellation closes the Promise executor; creation must outlive it to close a late client.
		coroutine.wrap(function()
			local Success, Error = pcall(CreateAndConnect)
			if not Success then
				Cleanup(true)
				reject(Error)
			end
		end)()
	end))
end

function ApiContext:__CreateWebSocketClient(Url)
	return HttpService:CreateWebStreamClient(Enum.WebStreamClientType.WebSocket, {
		Url = Url,
	})
end

function ApiContext:open(id)
	local url = ("%s/api/open/%s"):format(self.__baseUrl, id)

	return self:__Request("post", url, ""):andThen(function(body)
		if body.sessionId ~= self.__sessionId then
			return Promise.reject("Server changed ID")
		end

		return nil
	end)
end

function ApiContext:serialize(ids: { string })
	local url = ("%s/api/serialize"):format(self.__baseUrl)
	local request_body = Http.msgpackEncode({ sessionId = self.__sessionId, ids = ids })

	return self:__Request("post", url, request_body):andThen(function(response_body)
		if response_body.sessionId ~= self.__sessionId then
			return Promise.reject("Server changed ID")
		end

		assert(validateApiSerialize(response_body))

		return response_body
	end)
end

function ApiContext:refPatch(ids: { string })
	local url = ("%s/api/ref-patch"):format(self.__baseUrl)
	local request_body = Http.msgpackEncode({ sessionId = self.__sessionId, ids = ids })

	return self:__Request("post", url, request_body):andThen(function(response_body)
		if response_body.sessionId ~= self.__sessionId then
			return Promise.reject("Server changed ID")
		end

		assert(validateApiRefPatch(response_body))

		return response_body
	end)
end

return ApiContext
