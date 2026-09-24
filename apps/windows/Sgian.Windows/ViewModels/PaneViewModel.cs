using Sgian.Protocol;

namespace Sgian.Windows.ViewModels;

public sealed class PaneViewModel : ObservableObject
{
    private string _title;
    private string _state = "live";
    private string? _attention;
    private string? _mode;
    private bool _unattended;
    private string? _leaseHolder;
    private bool _leaseIsMine;
    private AgentPaneSpec? _agentSpec;
    private string? _projectName;
    private OutputTricks? _outputWarning;
    private AgentUsage? _usage;

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

    /// <summary>The agent's observed permission mode, or null when unknown.</summary>
    public string? Mode
    {
        get => _mode;
        set
        {
            if (Set(ref _mode, value))
            {
                Raise(nameof(Subtitle));
            }
        }
    }

    /// <summary>True when the agent runs tools without approval; the pane must show it.</summary>
    public bool Unattended
    {
        get => _unattended;
        set
        {
            if (Set(ref _unattended, value))
            {
                Raise(nameof(Subtitle));
            }
        }
    }

    /// <summary>Who holds this pane's keyboard, or null when unheld.</summary>
    public string? LeaseHolder
    {
        get => _leaseHolder;
        set
        {
            if (Set(ref _leaseHolder, value))
            {
                Raise(nameof(Subtitle));
                Raise(nameof(LeaseLabel));
            }
        }
    }

    public bool LeaseIsMine
    {
        get => _leaseIsMine;
        set
        {
            if (Set(ref _leaseIsMine, value))
            {
                Raise(nameof(Subtitle));
                Raise(nameof(LeaseLabel));
            }
        }
    }

    /// <summary>"⌨ you" / "⌨ holder", or empty when the pane is unheld.</summary>
    public string LeaseLabel =>
        LeaseHolder is null ? "" : $"\u2328 {(LeaseIsMine ? "you" : LeaseHolder)}";

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

    /// <summary>The project this pane serves, or null (ENHANCEMENTS "projects").</summary>
    public string? ProjectName
    {
        get => _projectName;
        set
        {
            if (Set(ref _projectName, value))
            {
                Raise(nameof(Subtitle));
            }
        }
    }

    /// <summary>Output-guard totals when the pane's output hid something, else null.</summary>
    public OutputTricks? OutputWarning
    {
        get => _outputWarning;
        set
        {
            if (Set(ref _outputWarning, value))
            {
                Raise(nameof(Subtitle));
                Raise(nameof(OutputWarningLabel));
                Raise(nameof(OutputWarningSummary));
            }
        }
    }

    /// <summary>What the session under this pane last said through the status line, or null.</summary>
    public AgentUsage? Usage
    {
        get => _usage;
        set
        {
            if (Set(ref _usage, value))
            {
                Raise(nameof(Subtitle));
                Raise(nameof(UsageLabel));
            }
        }
    }

    /// <summary>"Opus · 40% context · 5h 23% ↻ 1h10m" or empty.</summary>
    public string UsageLabel => Usage is null ? "" : Usage.Summary();

    /// <summary>"⚠ 3 hidden" or empty; the per-kind counts are in <see cref="OutputWarningSummary"/>.</summary>
    public string OutputWarningLabel =>
        OutputWarning is null ? "" : $"\u26A0 {OutputWarning.Total} hidden";

    public string OutputWarningSummary =>
        OutputWarning is null
            ? ""
            : $"Output hid something: {OutputWarning.Summary}" + (OutputWarning.Sample is null ? "" : $" · first seen: {OutputWarning.Sample}");

    public string Subtitle
    {
        get
        {
            var mode = Mode is null ? "" : Unattended ? $" · \u26A0 {Mode} (unattended)" : $" · {Mode}";
            var warning = OutputWarning is null ? "" : $" · {OutputWarningLabel}";
            var usage = Usage is null ? "" : $" · {UsageLabel}";
            var lease = (LeaseHolder is null ? "" : $" · {LeaseLabel}") + mode + warning + usage;
            var project = ProjectName is null ? "" : $"{ProjectName} · ";
            if (State == "ended")
            {
                return project + "Ended" + lease;
            }
            if (!IsAgent)
            {
                return project + (Attention is null ? "Terminal" : $"Terminal · {Attention.Replace('_', ' ')}") + lease;
            }
            var identity = AgentSpec is null
                ? "Agent"
                : string.Join(" · ", new[] { AgentSpec.Backend, AgentSpec.Model }
                    .Where(value => !string.IsNullOrWhiteSpace(value)));
            return project + (Attention is null ? identity : $"{identity} · {Attention.Replace('_', ' ')}") + lease;
        }
    }
}
