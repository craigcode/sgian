using Sgian.Protocol;

namespace Sgian.Windows.ViewModels;

public sealed class PaneViewModel : ObservableObject
{
    private string _title;
    private string _state = "live";
    private string? _attention;
    private AgentPaneSpec? _agentSpec;

    public PaneViewModel(Pane pane)
    {
        Id = pane.Id;
        Kind = pane.Kind;
        CreatedAtMilliseconds = pane.CreatedAtMilliseconds;
        _title = pane.Title;
    }

    public string Id { get; }
    public string Kind { get; }
    public ulong CreatedAtMilliseconds { get; }
    public bool IsAgent => Kind == "agent";
    public string Glyph => IsAgent ? "\uE8BD" : "\uE756";

    public string Title
    {
        get => _title;
        set => Set(ref _title, value);
    }

    public string State
    {
        get => _state;
        set
        {
            if (Set(ref _state, value))
            {
                Raise(nameof(Subtitle));
            }
        }
    }

    public string? Attention
    {
        get => _attention;
        set
        {
            if (Set(ref _attention, value))
            {
                Raise(nameof(Subtitle));
            }
        }
    }

    public AgentPaneSpec? AgentSpec
    {
        get => _agentSpec;
        set
        {
            if (Set(ref _agentSpec, value))
            {
                Raise(nameof(Subtitle));
            }
        }
    }

    public string Subtitle
    {
        get
        {
            if (State == "ended")
            {
                return "Ended";
            }
            if (!IsAgent)
            {
                return Attention is null ? "Terminal" : $"Terminal · {Attention.Replace('_', ' ')}";
            }
            var identity = AgentSpec is null
                ? "Agent"
                : string.Join(" · ", new[] { AgentSpec.Backend, AgentSpec.Model }
                    .Where(value => !string.IsNullOrWhiteSpace(value)));
            return Attention is null ? identity : $"{identity} · {Attention.Replace('_', ' ')}";
        }
    }
}
