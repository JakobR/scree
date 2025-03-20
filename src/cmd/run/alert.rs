/*

TODO: every hour, check if any alerts haven't been delivered yet and re-attempt delivery.
    rate limit? maybe max. 10 per hour, with a few seconds sleep in between.

when sending alerts, lock the row to make sure we send it only once: https://stackoverflow.com/a/52557413

*/
