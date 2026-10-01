using System.Text;

namespace Sgian.Protocol;

/// Limits allocation before a newline arrives. StreamReader.ReadLineAsync
/// buffers the entire line before the caller can inspect its length.
public sealed class BoundedLineReader(TextReader reader, int maximumCharacters = 8 * 1024 * 1024,
    bool leaveOpen = false) : IDisposable
{
    private readonly char[] _buffer = new char[4096];
    private int _offset;
    private int _count;

    public async Task<string?> ReadLineAsync(CancellationToken cancellationToken = default)
    {
        var line = new StringBuilder();
        while (true)
        {
            if (_offset == _count)
            {
                _count = await reader.ReadAsync(_buffer.AsMemory(), cancellationToken).ConfigureAwait(false);
                _offset = 0;
                if (_count == 0)
                {
                    if (line.Length == 0) return null;
                    throw new DaemonProtocolException("The daemon closed an incomplete message.");
                }
            }
            var newline = Array.IndexOf(_buffer, '\n', _offset, _count - _offset);
            var end = newline < 0 ? _count : newline;
            var length = end - _offset;
            if (length > maximumCharacters - line.Length)
                throw new DaemonProtocolException("The daemon sent an oversized message.");
            line.Append(_buffer, _offset, length);
            _offset = end;
            if (newline >= 0)
            {
                _offset++;
                if (line.Length > 0 && line[^1] == '\r') line.Length--;
                return line.ToString();
            }
        }
    }

    public void Dispose()
    {
        if (!leaveOpen) reader.Dispose();
    }
}
