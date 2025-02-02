sudo ifconfig en13 10.0.0.4 netmask 255.255.255.0 down

sudo route -n delete -net 10.0.0.0/24 10.0.0.1


# NOTE: 10.0.0.4 is configured as my ip in my mac for this network
sudo ifconfig en13 10.0.0.4 netmask 255.255.255.0 up

sudo route -n add  -net 10.0.0.0/24 10.0.0.1


