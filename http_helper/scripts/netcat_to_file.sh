while true ; do echo -e "HTTP/1.1 200 OK\n\nHello" | nc -l -p $1 1> output.txt 2>output.err; done

