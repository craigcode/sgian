using System.Text.Json;

namespace Sgian.Protocol;

public enum ChatMessageKind
{
    User,
    Assistant,
    Tool,
    ToolResult,
    Error,
    HistoryElided,
}

public sealed class ChatMessage
{
    public Guid Id { get; } = Guid.NewGuid();
    public ChatMessageKind Kind { get; init; }
    public string Text { get; set; } = "";
    public string? Title { get; init; }
    public string? Detail { get; init; }
    public string? ToolUseId { get; init; }
    public bool IsError { get; set; }
    public bool IsOpen { get; set; }

    public string Label => Kind switch
    {
        ChatMessageKind.User => "You",
        ChatMessageKind.Assistant => "Assistant",
        ChatMessageKind.Tool => Title ?? "Tool",
        ChatMessageKind.ToolResult => "Tool result",
        ChatMessageKind.Error => "Error",
        _ => "History",
    };
}

public sealed record PendingPermission(string RequestId, string ToolName, JsonElement? Input);

public sealed record AgentTurnSummary(string? Subtype, double? CostUsd, double? DurationMilliseconds)
{
    public string Label => string.Join(" · ", new[]
    {
        string.IsNullOrWhiteSpace(Subtype) ? null : Subtype.Replace('_', ' '),
        CostUsd is null ? null : $"${CostUsd:0.0000}",
        DurationMilliseconds is null ? null : $"{DurationMilliseconds / 1000:0.0}s",
    }.Where(value => value is not null));
}

public sealed class AgentChatState
{
    private const int MaximumMessages = 500;

    public event EventHandler? Changed;

    public string? SessionId { get; private set; }
    public string? Model { get; private set; }
    public List<ChatMessage> Messages { get; } = [];
    public bool Busy { get; private set; }
    public PendingPermission? PendingPermission { get; private set; }
    public AgentTurnSummary? LastTurn { get; private set; }
    public bool Exited { get; private set; }
    public int? ExitCode { get; private set; }
    public ulong LastSequence { get; private set; }

    public void Replay(IEnumerable<JsonElement> events)
    {
        foreach (var item in events)
        {
            Apply(item, notify: false);
        }
        Changed?.Invoke(this, EventArgs.Empty);
    }

    public void Apply(JsonElement item) => Apply(item, notify: true);

    public void AppendUserMessage(string text)
    {
        Messages.Add(new ChatMessage { Kind = ChatMessageKind.User, Text = text });
        Busy = true;
        CapMessages();
        Changed?.Invoke(this, EventArgs.Empty);
    }

    public void RemoveLastUserMessage(string text)
    {
        var index = Messages.FindLastIndex(message =>
            message.Kind == ChatMessageKind.User && message.Text == text);
        if (index >= 0)
        {
            Messages.RemoveAt(index);
            Busy = false;
            Changed?.Invoke(this, EventArgs.Empty);
        }
    }

    public void MarkPaneEnded(int? exitCode = null)
    {
        CloseOpenAssistant();
        Busy = false;
        PendingPermission = null;
        Exited = true;
        ExitCode = exitCode;
        Changed?.Invoke(this, EventArgs.Empty);
    }

    private void Apply(JsonElement item, bool notify)
    {
        if (item.ValueKind != JsonValueKind.Object || !item.TryGetProperty("kind", out var kindValue))
        {
            return;
        }
        if (item.TryGetProperty("seq", out var sequenceValue) && sequenceValue.TryGetUInt64(out var sequence))
        {
            if (sequence <= LastSequence)
            {
                return;
            }
            LastSequence = sequence;
        }

        switch (kindValue.GetString())
        {
            case "session":
                SessionId = Text(item, "session_id") ?? SessionId;
                Model = Text(item, "model") ?? Model;
                Exited = false;
                ExitCode = null;
                break;
            case "message_start":
                CloseOpenAssistant();
                Messages.Add(new ChatMessage { Kind = ChatMessageKind.Assistant, IsOpen = true });
                Busy = true;
                break;
            case "text_delta":
                var text = Text(item, "text");
                if (!string.IsNullOrEmpty(text))
                {
                    var open = Messages.FindLast(message =>
                        message.Kind == ChatMessageKind.Assistant && message.IsOpen);
                    if (open is null)
                    {
                        open = new ChatMessage { Kind = ChatMessageKind.Assistant, IsOpen = true };
                        Messages.Add(open);
                    }
                    open.Text += text;
                    Busy = true;
                }
                break;
            case "message_complete":
                CloseOpenAssistant();
                break;
            case "tool_use":
                CloseOpenAssistant();
                Messages.Add(new ChatMessage
                {
                    Kind = ChatMessageKind.Tool,
                    Title = Text(item, "name") ?? "Tool",
                    Detail = Pretty(item, "input"),
                    ToolUseId = Text(item, "id"),
                });
                break;
            case "tool_result":
                var toolId = Text(item, "tool_use_id");
                var content = ToolContent(item);
                var isError = item.TryGetProperty("is_error", out var errorValue) && errorValue.ValueKind == JsonValueKind.True;
                var tool = Messages.FindLast(message =>
                    message.Kind == ChatMessageKind.Tool && message.ToolUseId == toolId && message.Text.Length == 0);
                if (tool is not null)
                {
                    tool.Text = content;
                    tool.IsError = isError;
                }
                else
                {
                    Messages.Add(new ChatMessage
                    {
                        Kind = ChatMessageKind.ToolResult,
                        Text = content,
                        ToolUseId = toolId,
                        IsError = isError,
                    });
                }
                break;
            case "permission_request":
                var requestId = Text(item, "request_id");
                if (requestId is not null)
                {
                    PendingPermission = new PendingPermission(
                        requestId,
                        Text(item, "tool_name") ?? "Tool",
                        item.TryGetProperty("input", out var input) ? input.Clone() : null);
                }
                break;
            case "permission_resolved":
                if (Text(item, "request_id") == PendingPermission?.RequestId)
                {
                    PendingPermission = null;
                    var reason = Text(item, "reason");
                    if (reason is "closed" or "process_exit")
                    {
                        Busy = false;
                    }
                }
                break;
            case "turn_complete":
                CloseOpenAssistant();
                Busy = false;
                PendingPermission = null;
                LastTurn = new AgentTurnSummary(
                    Text(item, "subtype"), Number(item, "cost_usd"), Number(item, "duration_ms"));
                break;
            case "error":
                Messages.Add(new ChatMessage
                {
                    Kind = ChatMessageKind.Error,
                    Text = Text(item, "message") ?? "Agent error",
                    IsError = true,
                });
                break;
            case "process_exit":
                CloseOpenAssistant();
                Busy = false;
                PendingPermission = null;
                Exited = true;
                ExitCode = item.TryGetProperty("exit_code", out var exit) && exit.TryGetInt32(out var code)
                    ? code
                    : null;
                break;
        }
        CapMessages();
        if (notify)
        {
            Changed?.Invoke(this, EventArgs.Empty);
        }
    }

    private void CloseOpenAssistant()
    {
        var message = Messages.FindLast(item =>
            item.Kind == ChatMessageKind.Assistant && item.IsOpen);
        if (message is not null)
        {
            message.IsOpen = false;
        }
    }

    private void CapMessages()
    {
        if (Messages.Count <= MaximumMessages)
        {
            return;
        }
        var removed = Messages.Count - MaximumMessages + 1;
        Messages.RemoveRange(0, removed);
        Messages.Insert(0, new ChatMessage
        {
            Kind = ChatMessageKind.HistoryElided,
            Text = $"{removed} earlier messages are not shown",
        });
    }

    private static string? Text(JsonElement item, string property) =>
        item.TryGetProperty(property, out var value) && value.ValueKind == JsonValueKind.String
            ? value.GetString()
            : null;

    private static double? Number(JsonElement item, string property) =>
        item.TryGetProperty(property, out var value) && value.TryGetDouble(out var result)
            ? result
            : null;

    private static string? Pretty(JsonElement item, string property) =>
        item.TryGetProperty(property, out var value)
            ? JsonSerializer.Serialize(value, new JsonSerializerOptions { WriteIndented = true })
            : null;

    private static string ToolContent(JsonElement item)
    {
        if (!item.TryGetProperty("content", out var content) || content.ValueKind == JsonValueKind.Null)
        {
            return "";
        }
        if (content.ValueKind == JsonValueKind.String)
        {
            return content.GetString() ?? "";
        }
        if (content.ValueKind == JsonValueKind.Array)
        {
            return string.Join("\n", content.EnumerateArray().Select(block =>
            {
                if (block.ValueKind == JsonValueKind.String)
                {
                    return block.GetString();
                }
                return block.TryGetProperty("text", out var text) && text.ValueKind == JsonValueKind.String
                    ? text.GetString()
                    : JsonSerializer.Serialize(block);
            }).Where(value => value is not null));
        }
        return JsonSerializer.Serialize(content, new JsonSerializerOptions { WriteIndented = true });
    }
}
