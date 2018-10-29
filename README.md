# Full Node

> Just because you call something a blockchain, that doesn't mean you aren't subject to normal engineering laws. 

## Internal Design

glossary:

* blockchains: the current blockchain and possibly different known forks.
* clock: general time tracking to know the time in blockchain unit (epoch/slot)
* tip: the current fork that is considered the correct one, related to consensus algorithm.

General tasks:

* Network task: Handle new connections, and lowlevel queries. mostly parsing and routing them to
  block, client or transaction tasks.

* Block task: Handle all the blocks reception from nodes and leadership thread.
  On reception of external blocks, validate the block, and on succesful validation
  append to the blockchains, check if the blockchain tip need change. On internal block
  reception, same except without need for validation, broadcast change of tip back to network thread

* Leadership task: Wait for each new slot, and evaluate whether or not this node is
  a slot leader. If yes, then create a new block (with a set of known
  transactions) referencing the latest known and agreed block in the blockchain,
  then send it to the block thread for processing (appending to blockchain structure, then broadcasting)

* Client task: receive block header/body queries (e.g. Get Block 1 to 2000), and is in charge
  of in accord with the blockchains, reply to the client.

* Transaction task: receive new transaction from the network, validate transaction and handle duplicates.
  Also broadcast to other nodes new (valid) transaction received.