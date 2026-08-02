#include <fcntl.h>
#include <signal.h>
#include <unistd.h>

int main(int argc, char **argv) {
    if (argc == 2) {
        int fd = open(argv[1], O_RDONLY);
        if (fd < 0 || dup2(fd, 9) < 0) {
            return 1;
        }
        if (fd != 9) {
            close(fd);
        }
    } else if (argc != 1) {
        return 2;
    }

    for (;;) {
        pause();
    }
}
