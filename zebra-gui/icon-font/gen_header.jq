def convert(base):
  def stream:
    recurse(if . >= base then ./base|floor else empty end) | . % base ;
  [stream] | reverse
  | if   base <  10 then map(tostring) | join("")
    elif base <= 36 then map(if . < 10 then 48 + . else . + 87 end) | implode
    else error("base too large")
    end;
    
(.glyphs | sort_by(.code))[] | (
    "pub const ICON_" +
    (.css | ascii_upcase | (split("-1")[0] | split("-") | join("_"))) +
    ": &str = " + (.code | "\"\\u{"+convert(16)+"}\"") + ";")
