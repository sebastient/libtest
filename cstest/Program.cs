// C# validation of the MULTI-MODULE BoltFFI shape: two generated modules
// (Amod, Bmod) in one process, two native libraries, one shared
// implementation (liba). A-objects cross between modules as raw u64
// handles; all lifetime operations execute inside liba.
using Amod;
using static Bmod.Bmod;

static void Check(bool cond, string msg)
{
    if (cond) { Console.WriteLine($"  ok   {msg}"); }
    else { Console.WriteLine($"  FAIL {msg}"); Environment.Exit(1); }
}

// Fill pattern is (seed + offset) % 256 over 4096 bytes: every byte value
// appears equally often, so the checksum is seed-independent.
const ulong Expected = 16UL * (255UL * 256UL / 2UL);

using (var src = new AmSource(40))
{
    Check(src.Id() == 40 && src.Counter() == 0, "constructor and accessors (Amod)");
    Check(ProcessSource(src.RawHandle()) == 42, "Bmod processes Amod's object via raw handle");
    Check(src.Counter() == 2, "Amod sees Bmod's increments (same liba object)");

    src.Fill(1);
    Check(ChecksumSource(src.RawHandle()) == Expected, "cross-module zero-copy checksum");

    try { src.TryFill(0); Check(false, "TryFill(0) should throw"); }
    catch (AmErrorException e) { Check(e.Error == AmError.InvalidArgument, "error enum maps to typed exception"); }

    using (var frame = src.Frame())
    {
        ulong borrowed = frame.RawRetained();
        Check(FrameChecksum(borrowed) == Expected, "Bmod checksums a borrowed frame handle");
        Check(ConsumeFrameChecksum(borrowed) == Expected, "ownership transfer: consumed in Bmod, released inside liba");

        src.Fill(2); // COW detaches the producer
        Check(frame.FirstByte() == 1, "COW snapshot isolation survives module hops");

        ulong roundTrip = frame.RawRetained();
        using var rewrapped = AmFrame.FromRaw(roundTrip);
        Check(rewrapped.FirstByte() == 1, "retained handle re-wrapped by Amod");
    }

    // Platform storage descriptor (kind 2 = IOSurface on this host).
    src.SetStorage(2);
    src.Fill(5);
    using (var sf = src.Frame())
    {
        var desc = sf.ExportDesc();
        Check(desc.Kind == 2 && desc.Id != 0, "IOSurface descriptor crosses as a record");
    }
    try { src.SetStorage(99); Check(false, "unknown storage kind should throw"); }
    catch (AmErrorException e) { Check(e.Error == AmError.InvalidArgument, "unknown storage kind rejected"); }

    // Async: Task-based, over the same completion-callback machinery.
    using (var cap = await src.Capture())
    {
        Check(cap.FirstByte() == 0xCA, "Task-based async capture (Amod)");
    }
    ulong asyncChecksum = await CaptureChecksum(src.RawHandle());
    Check(asyncChecksum == Expected, "cross-module async Task (Bmod over Amod's object)");
}

Console.WriteLine("CSHARP RESULT: PASS");
