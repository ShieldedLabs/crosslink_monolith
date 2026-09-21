# A Visual Lexicon for TFL Book

## The Last Final Snapshot Rule

> **Last Final Snapshot rule:** $\snapshotlf{H} \preceq_{\bc} H$.

> For a bft‑block or bft‑proposal $B$, define $$
  \begin{array}{rl}
  \hphantom{\LF(H)}\snapshot(B) &\!\!\!\!:= \begin{cases}
    \Origin_{\bc},&\if B\dot\headersbc = \null \\
    B\dot\headersbc[0] \trunc_{\bc}^1,&\otherwise
  \end{cases}
  \end{array}
  $$

Note that the type of $B$ in the quoted definition is a BFT Finality Certificate.

```mermaid
graph TD
  %% Node Categories
  classDef protocol stroke-width:1px
  class powProto,bftProto protocol;

  classDef pow stroke-width:1px
  classDef powAlt stroke:grey,stroke-width:1px,stroke-dasharray:2,2
  classDef bft stroke-width:2px
  classDef elidedNode stroke-width:0px,fill:none

  %% Nodes
  subgraph powProto [Proof of Work blocks]
    B0(["`$$B_0$$`"]):::pow
    B1(["`$$B_1$$`"]):::pow
    B2(["`$$B_2$$`"]):::pow
    B3(["`$$B_3$$`"]):::pow

    B2alt(["`$$B_2'$$`"]):::powAlt
    B1alt(["`$$B_1'$$`"]):::powAlt
  end

  subgraph bftProto [BFT Finality Certificates]
    FIN_0("`$$FIN_0$$`"):::bft
    FIN_1("`$$FIN_1$$`"):::bft
    FIN_2("`$$FIN_2$$`"):::bft
  end

  %% "Out of view" node indicators:
  powDots(["…"]):::elidedNode
  bftDots(["…"]):::elidedNode

  %% PoW Edges
  B3 --> B2
  B2 --> B1
  B1 --> B0
  B0 --> powDots

  B2alt --> B1alt
  B1alt --> B0

  %% Finality certificate sequence
  FIN_2 --> FIN_1
  FIN_1 --> FIN_0
  FIN_0 --> bftDots

  %% Finality snapshots
  FIN_2 == snapshot ==> B2
  FIN_1 == snapshot ==> B0
  FIN_0 == snapshot ==> powDots
```

> For a bc‑block $H$, define $$
  \begin{array}{rl}
  \hphantom{\snapshot(B)}\LF(H) &\!\!\!\!:= \bftlastfinal(H\dot\contextbft) \\
                  \candidate(H) &\!\!\!\!:= \lastcommonancestor(\snapshotlf{H}, H \trunc_{\bc}^\sigma)
  \end{array}
  $$
