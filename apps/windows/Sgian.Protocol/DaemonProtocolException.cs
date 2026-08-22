namespace Sgian.Protocol;

public sealed class DaemonProtocolException : Exception
{
    public DaemonProtocolException(string message) : base(message) { }

    public DaemonProtocolException(string message, Exception innerException)
        : base(message, innerException) { }
}
