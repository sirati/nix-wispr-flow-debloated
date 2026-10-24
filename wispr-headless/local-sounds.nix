# The sounds of local dictations: two soft notes going up when one starts,
# the same two going down when it stops. They differ from Wispr's own so
# the key pressed can be heard, and are generated here rather than copied.
{ runCommand, sox }:
runCommand "wispr-headless-local-sounds" { nativeBuildInputs = [ sox ]; } ''
  mkdir -p $out
  # One note: a sine with its octave, a 3 ms attack and a quick fall.
  note() {
    sox -n -r 44100 -c 1 -b 16 "$2" synth 0.075 sine "$1" sine "$(( $1 * 2 ))" \
      remix 1v0.8,2v0.2 fade h 0.003 0.075 0.06 vol 0.175
  }
  note 784 low.wav
  note 1175 high.wav
  sox low.wav high.wav $out/local-start.wav
  sox high.wav low.wav $out/local-stop.wav
''
